//! KheishStack command handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use rand::RngCore as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const STACK_API_VERSION: &str = "kheish.ai/v1alpha1";
const STACK_KIND: &str = "KheishStack";
const LEDGER_VERSION: u32 = 1;
const LEDGER_DIR: &str = "kheish-apply";
const LEDGER_FILE: &str = "ledger.json";
pub const STACK_MANIFEST_BODY_LIMIT_BYTES: usize = 256 * 1024;
const STACK_MAX_RESOURCE_COUNT: usize = 512;

#[async_trait::async_trait]
pub(crate) trait StackControlPlane {
    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned + Send;

    async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
    where
        T: DeserializeOwned + Send;

    async fn post_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + Sync + ?Sized,
        T: DeserializeOwned + Send;

    async fn put_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + Sync + ?Sized,
        T: DeserializeOwned + Send;

    async fn delete_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned + Send;

    async fn create_secret_if_absent(
        &self,
        slot: &str,
        record: &kheish_auth::AuthSlotRecord,
    ) -> Result<kheish_auth::AuthSlotStatus> {
        let encoded = url_encode_path_segment(slot);
        let path = format!("/v1/runtime/secrets/{encoded}");
        if self
            .get_json_optional::<kheish_auth::AuthSlotStatus>(&path)
            .await?
            .is_some()
        {
            bail!("secret `{slot}` already exists");
        }
        self.post_json("/v1/runtime/secrets", record).await
    }

    async fn put_connector_if_absent(
        &self,
        kind: &str,
        name: &str,
        spec: &Value,
    ) -> Result<crate::ConnectorView> {
        let path = format!(
            "/v1/runtime/connectors/{}/{}",
            url_encode_path_segment(kind),
            url_encode_path_segment(name)
        );
        if self
            .get_json_optional::<crate::ConnectorView>(&path)
            .await?
            .is_some()
        {
            bail!("{kind} connector {name} already exists");
        }
        self.put_json(&path, spec).await
    }

    async fn create_persona_if_absent(
        &self,
        request: &crate::CreatePersonaRequest,
    ) -> Result<crate::PersonaView> {
        let persona_id = request
            .persona_id
            .as_deref()
            .ok_or_else(|| anyhow!("stack persona create requires persona_id"))?;
        let path = format!("/v1/personas/{}", url_encode_path_segment(persona_id));
        if self
            .get_json_optional::<crate::PersonaView>(&path)
            .await?
            .is_some()
        {
            bail!("persona {persona_id} already exists");
        }
        self.post_json("/v1/personas", request).await
    }

    async fn create_session_if_absent(
        &self,
        request: &crate::CreateSessionRequest,
    ) -> Result<crate::SessionView> {
        let session_id = request
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow!("stack session create requires session_id"))?;
        let path = format!("/v1/sessions/{}", url_encode_path_segment(session_id));
        if self
            .get_json_optional::<crate::SessionView>(&path)
            .await?
            .is_some()
        {
            bail!("session {session_id} already exists");
        }
        self.post_json("/v1/sessions", request).await
    }

    async fn create_schedule_if_absent(
        &self,
        request: &crate::ScheduleCreateRequest,
    ) -> Result<crate::ScheduleView> {
        let live = self
            .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
            .await?;
        if live.iter().any(|schedule| schedule.name == request.name) {
            bail!("schedule {} already exists", request.name);
        }
        self.post_json("/v1/schedules", request).await
    }

    async fn create_playbook_version_if_absent(
        &self,
        request: &crate::CreatePlaybookRequest,
    ) -> Result<crate::PlaybookView> {
        let playbook_id = &request.manifest.playbook_id;
        let version = &request.manifest.version;
        let path = format!("/v1/playbooks/{}", url_encode_path_segment(playbook_id));
        if self
            .get_json_optional::<crate::PlaybookView>(&path)
            .await?
            .is_some_and(|view| {
                view.versions
                    .iter()
                    .any(|candidate| candidate.version == *version)
            })
        {
            bail!("playbook {playbook_id}@{version} already exists");
        }
        self.post_json("/v1/playbooks", request).await
    }
}

pub(crate) struct StateStackControlPlane<M> {
    state: Arc<crate::DaemonState<M>>,
}

impl<M> StateStackControlPlane<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) fn new(state: Arc<crate::DaemonState<M>>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl<M> StackControlPlane for StateStackControlPlane<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned + Send,
    {
        let segments = decoded_path_segments(path)?;
        let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match parts.as_slice() {
            ["v1", "runtime"] => encode_response(self.state.runtime_settings().await),
            ["v1", "runtime", "secrets", slot] => {
                encode_response(self.state.auth_status(slot).await?)
            }
            ["v1", "runtime", "connectors", kind, name] => {
                let view = self
                    .state
                    .connector(kind, name)
                    .await
                    .map(crate::ConnectorView::from)
                    .ok_or_else(|| anyhow!("unknown connector {kind}/{name}"))?;
                encode_response(view)
            }
            ["v1", "personas", persona_id] => encode_response(crate::PersonaView::from(
                self.state.get_persona_record(persona_id).await?,
            )),
            ["v1", "sessions", session_id] => {
                let agent_id = self.state.agent_id_for_session(session_id).await?;
                encode_response(self.state.session_view(session_id, &agent_id).await?)
            }
            ["v1", "schedules"] => encode_response(self.state.list_schedules(None).await?),
            ["v1", "playbooks", playbook_id] => {
                encode_response(self.state.get_playbook(playbook_id, None).await?)
            }
            _ => bail!("unsupported stack control-plane GET path {path}"),
        }
    }

    async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
    where
        T: DeserializeOwned + Send,
    {
        let segments = decoded_path_segments(path)?;
        let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match parts.as_slice() {
            ["v1", "runtime", "secrets", slot] => {
                let status = self
                    .state
                    .auth_manager()
                    .status(&kheish_auth::AuthSlotId::new((*slot).to_string()))
                    .await?;
                status.map(encode_response).transpose()
            }
            ["v1", "runtime", "connectors", kind, name] => self
                .state
                .connector(kind, name)
                .await
                .map(crate::ConnectorView::from)
                .map(encode_response)
                .transpose(),
            ["v1", "personas", persona_id] => match self.state.get_persona_record(persona_id).await
            {
                Ok(record) => encode_response(crate::PersonaView::from(record)).map(Some),
                Err(error) if error.to_string().contains("unknown persona") => Ok(None),
                Err(error) => Err(error),
            },
            ["v1", "sessions", session_id] => {
                match self.state.agent_id_for_session(session_id).await {
                    Ok(agent_id) => {
                        encode_response(self.state.session_view(session_id, &agent_id).await?)
                            .map(Some)
                    }
                    Err(error) if error.to_string().contains("unknown session") => Ok(None),
                    Err(error) => Err(error),
                }
            }
            ["v1", "playbooks", playbook_id] => {
                match self.state.get_playbook(playbook_id, None).await {
                    Ok(view) => encode_response(view).map(Some),
                    Err(error) if error.to_string().contains("unknown playbook") => Ok(None),
                    Err(error) => Err(error),
                }
            }
            _ => self.get_json(path).await.map(Some),
        }
    }

    async fn post_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + Sync + ?Sized,
        T: DeserializeOwned + Send,
    {
        let segments = decoded_path_segments(path)?;
        let body = serde_json::to_value(body)?;
        let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match parts.as_slice() {
            ["v1", "playbooks", "validate"] => {
                let request = serde_json::from_value::<crate::ValidatePlaybookRequest>(body)?;
                encode_response(self.state.validate_playbook(request))
            }
            ["v1", "runtime", "secrets"] => {
                let request = serde_json::from_value::<kheish_auth::AuthSlotRecord>(body)?;
                encode_response(self.state.put_auth_record(request).await?)
            }
            ["v1", "runtime", "permission-mode"] => {
                let request = serde_json::from_value::<crate::SetPermissionModeRequest>(body)?;
                encode_response(
                    self.state
                        .set_permission_mode(request.mode, request.expected_revision)
                        .await?,
                )
            }
            ["v1", "runtime", "debug-level"] => {
                let request = serde_json::from_value::<crate::SetDebugLevelRequest>(body)?;
                encode_response(
                    self.state
                        .set_debug_level(request.level, request.expected_revision)
                        .await?,
                )
            }
            ["v1", "personas"] => {
                let request = serde_json::from_value::<crate::CreatePersonaRequest>(body)?;
                encode_response(crate::PersonaView::from(
                    self.state
                        .create_persona_record(
                            request.persona_id,
                            request.display_name,
                            request.soul,
                            request.capability_scope.unwrap_or_default(),
                            request.default_skills.unwrap_or_default(),
                            request.metadata.unwrap_or(Value::Null),
                        )
                        .await?,
                ))
            }
            ["v1", "sessions"] => {
                let request = serde_json::from_value::<crate::CreateSessionRequest>(body)?;
                encode_response(self.state.create_session(request).await?)
            }
            ["v1", "sessions", session_id, "persona"] => {
                let request = serde_json::from_value::<crate::SetSessionPersonaRequest>(body)?;
                encode_response(
                    self.state
                        .set_session_persona_view(session_id, &request.persona_id)
                        .await?,
                )
            }
            ["v1", "sessions", session_id, "capability-scope"] => {
                let request =
                    serde_json::from_value::<crate::SetSessionCapabilityScopeRequest>(body)?;
                encode_response(
                    self.state
                        .set_session_capability_scope(session_id, request.capability_scope)
                        .await?,
                )
            }
            ["v1", "sessions", session_id, "credential-scope"] => {
                let request =
                    serde_json::from_value::<crate::SetSessionCredentialScopeRequest>(body)?;
                encode_response(
                    self.state
                        .set_session_credential_scope(session_id, request.credential_scope)
                        .await?,
                )
            }
            ["v1", "sessions", session_id, "route-policy"] => {
                let request = serde_json::from_value::<crate::SetSessionRoutePolicyRequest>(body)?;
                encode_response(
                    self.state
                        .set_session_route_policy(session_id, request.route_policy)
                        .await?,
                )
            }
            ["v1", "sessions", session_id, "operator"] => {
                let request =
                    serde_json::from_value::<crate::SetSessionOperatorConfigRequest>(body)?;
                encode_response(
                    self.state
                        .set_session_operator_config(session_id, Some(request.operator))
                        .await?,
                )
            }
            ["v1", "sessions", session_id, "reply-targets"] => {
                let request = serde_json::from_value::<crate::SetSessionReplyTargetsRequest>(body)?;
                let reply_targets = request
                    .reply_targets
                    .into_iter()
                    .map(crate::SessionReplyTargetRequest::into_reply_handle)
                    .collect::<Vec<_>>();
                self.state
                    .validate_persisted_reply_targets(&reply_targets)?;
                encode_response(
                    self.state
                        .set_session_reply_targets(session_id, reply_targets)
                        .await?,
                )
            }
            ["v1", "schedules"] => {
                let request = serde_json::from_value::<crate::ScheduleCreateRequest>(body)?;
                encode_response(self.state.create_schedule(request).await?)
            }
            ["v1", "schedules", schedule_id, "cancel"] => {
                encode_response(crate::ScheduleMutationResponse {
                    schedule: self.state.cancel_schedule(schedule_id).await?,
                })
            }
            ["v1", "playbooks"] => {
                let request = serde_json::from_value::<crate::CreatePlaybookRequest>(body)?;
                encode_response(self.state.create_playbook(request).await?)
            }
            ["v1", "playbooks", playbook_id, "publish"] => {
                let request = serde_json::from_value::<crate::PublishPlaybookRequest>(body)?;
                encode_response(self.state.publish_playbook(playbook_id, request).await?)
            }
            ["v1", "sessions", session_id, "end"] => {
                let request = serde_json::from_value::<crate::EndSessionRequest>(body)?;
                encode_response(self.state.end_session(session_id, request.reason).await?)
            }
            _ => bail!("unsupported stack control-plane POST path {path}"),
        }
    }

    async fn put_json<B, T>(&self, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + Sync + ?Sized,
        T: DeserializeOwned + Send,
    {
        let segments = decoded_path_segments(path)?;
        let body = serde_json::to_value(body)?;
        let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match parts.as_slice() {
            ["v1", "runtime", "connectors", kind, name] if *kind == "http" => {
                let request = serde_json::from_value::<crate::PutHttpConnectorRequest>(body)?;
                let config =
                    build_stack_http_connector_config(self.state.as_ref(), name, request).await?;
                self.state.put_http_connector(config).await?;
                let view = self
                    .state
                    .connector(kind, name)
                    .await
                    .map(crate::ConnectorView::from)
                    .ok_or_else(|| anyhow!("unknown connector {kind}/{name}"))?;
                encode_response(view)
            }
            ["v1", "personas", persona_id] => {
                let request = serde_json::from_value::<crate::UpdatePersonaRequest>(body)?;
                encode_response(crate::PersonaView::from(
                    self.state
                        .update_persona_record(
                            persona_id,
                            request.display_name,
                            request.soul,
                            request.capability_scope,
                            request.default_skills,
                            request.metadata,
                        )
                        .await?,
                ))
            }
            ["v1", "runtime", "connectors", kind, _] => {
                bail!(
                    "KheishStack v1alpha1 does not support daemon apply for connector kind `{kind}`"
                )
            }
            _ => bail!("unsupported stack control-plane PUT path {path}"),
        }
    }

    async fn delete_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned + Send,
    {
        let segments = decoded_path_segments(path)?;
        let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match parts.as_slice() {
            ["v1", "runtime", "connectors", kind, name] => {
                let deleted = self.state.delete_connector(kind, name).await?;
                encode_response(json!({ "accepted": deleted }))
            }
            ["v1", "runtime", "secrets", slot] => {
                let deleted = self.state.delete_auth_slot(slot).await?;
                encode_response(json!({ "accepted": deleted }))
            }
            _ => bail!("unsupported stack control-plane DELETE path {path}"),
        }
    }

    async fn create_secret_if_absent(
        &self,
        _slot: &str,
        record: &kheish_auth::AuthSlotRecord,
    ) -> Result<kheish_auth::AuthSlotStatus> {
        self.state.put_auth_record_if_absent(record.clone()).await
    }

    async fn put_connector_if_absent(
        &self,
        kind: &str,
        name: &str,
        spec: &Value,
    ) -> Result<crate::ConnectorView> {
        match kind {
            "http" => {
                let request =
                    serde_json::from_value::<crate::PutHttpConnectorRequest>(spec.clone())?;
                let config =
                    build_stack_http_connector_config(self.state.as_ref(), name, request).await?;
                self.state.put_http_connector_if_absent(config).await?;
                let view = self
                    .state
                    .connector(kind, name)
                    .await
                    .map(crate::ConnectorView::from)
                    .ok_or_else(|| anyhow!("unknown connector {kind}/{name}"))?;
                Ok(view)
            }
            _ => bail!(
                "KheishStack v1alpha1 does not support daemon apply for connector kind `{kind}`"
            ),
        }
    }

    async fn create_persona_if_absent(
        &self,
        request: &crate::CreatePersonaRequest,
    ) -> Result<crate::PersonaView> {
        encode_response(crate::PersonaView::from(
            self.state
                .create_persona_record(
                    request.persona_id.clone(),
                    request.display_name.clone(),
                    request.soul.clone(),
                    request.capability_scope.clone().unwrap_or_default(),
                    request.default_skills.clone().unwrap_or_default(),
                    request.metadata.clone().unwrap_or(Value::Null),
                )
                .await?,
        ))
    }

    async fn create_session_if_absent(
        &self,
        request: &crate::CreateSessionRequest,
    ) -> Result<crate::SessionView> {
        encode_response(self.state.create_session_if_absent(request.clone()).await?)
    }

    async fn create_schedule_if_absent(
        &self,
        request: &crate::ScheduleCreateRequest,
    ) -> Result<crate::ScheduleView> {
        encode_response(
            self.state
                .create_schedule_if_name_absent(request.clone())
                .await?,
        )
    }

    async fn create_playbook_version_if_absent(
        &self,
        request: &crate::CreatePlaybookRequest,
    ) -> Result<crate::PlaybookView> {
        encode_response(
            self.state
                .create_playbook_if_version_absent(request.clone())
                .await?,
        )
    }
}

fn encode_response<T, S>(value: S) -> Result<T>
where
    T: DeserializeOwned,
    S: Serialize,
{
    serde_json::from_value(serde_json::to_value(value)?).context("failed to encode stack response")
}

fn decoded_path_segments(path: &str) -> Result<Vec<String>> {
    path.trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            urlencoding::decode(segment)
                .map(|decoded| decoded.into_owned())
                .with_context(|| format!("failed to decode path segment `{segment}`"))
        })
        .collect()
}

async fn build_stack_http_connector_config<M>(
    state: &crate::DaemonState<M>,
    name: &str,
    request: crate::PutHttpConnectorRequest,
) -> Result<crate::HttpInputConnectorConfig>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    let session_policy = request.session_policy.unwrap_or_default().normalized();
    if let Some(persona_id) = session_policy.persona_id.as_deref() {
        state.get_persona_record(persona_id).await?;
    }
    let bearer_token = request.bearer_token.unwrap_or_default();
    let hmac_secret = request.hmac_secret.unwrap_or_default();
    let default_reply_targets = request.default_reply_targets.unwrap_or_default();
    state.validate_persisted_reply_targets(&default_reply_targets)?;
    Ok(crate::HttpInputConnectorConfig {
        name: name.to_string(),
        fixed_session_id: normalized_optional_string(request.fixed_session_id, "fixed_session_id")?,
        actor_id: normalized_optional_string(request.actor_id, "actor_id")?,
        bearer_token: normalized_optional_string(bearer_token.value, "bearer_token.value")?,
        bearer_token_env: normalized_optional_string(bearer_token.env, "bearer_token.env")?,
        bearer_token_secret_ref: normalized_optional_string(
            bearer_token.secret_ref,
            "bearer_token.secret_ref",
        )?,
        hmac_secret: normalized_optional_string(hmac_secret.value, "hmac_secret.value")?,
        hmac_secret_env: normalized_optional_string(hmac_secret.env, "hmac_secret.env")?,
        hmac_secret_secret_ref: normalized_optional_string(
            hmac_secret.secret_ref,
            "hmac_secret.secret_ref",
        )?,
        allow_unauthenticated_ingress: request.allow_unauthenticated_ingress.unwrap_or(false),
        require_hmac_signature: request.require_hmac_signature.unwrap_or(false),
        signature_max_age_secs: request
            .signature_max_age_secs
            .unwrap_or_else(crate::connectors::http_default_signature_max_age_secs),
        require_idempotency_key: request.require_idempotency_key.unwrap_or(true),
        ingress_events_per_second: request
            .ingress_events_per_second
            .unwrap_or_else(crate::connectors::http_default_ingress_events_per_second),
        allow_payload_reply_targets: request.allow_payload_reply_targets.unwrap_or(false),
        default_reply_targets,
        default_binding_keys: request.default_binding_keys.unwrap_or_default(),
        session_policy,
    })
}

fn normalized_optional_string(value: Option<String>, field: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("{field} cannot be empty");
            }
            Ok(trimmed.to_string())
        })
        .transpose()
}

pub fn generic_stack_template(name: &str) -> String {
    render_generic_stack_template(name)
}

pub(crate) fn stack_ledger_path(state_root: &Path) -> PathBuf {
    state_root.join(LEDGER_DIR).join(LEDGER_FILE)
}

fn render_generic_stack_template(name: &str) -> String {
    format!(
        r#"apiVersion: {STACK_API_VERSION}
kind: {STACK_KIND}
metadata:
  name: {name}
spec:
  apply:
    strict_scopes: true
    restart_policy: plan_only
  requires:
    secrets: []
  runtime: {{}}
  connectors: []
  personas: []
  sessions: []
  schedules: []
  playbooks: []
  verification: []
"#
    )
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StackApplyOptions {
    pub(crate) dry_run: bool,
    pub(crate) force_restart: bool,
    pub(crate) allow_secret_env: bool,
    pub(crate) prune: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StackImportOptions {
    pub(crate) resources: Vec<String>,
    pub(crate) allow_secret_env: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StackDownOptions {
    pub(crate) yes: bool,
}

pub(crate) async fn validate_stack_context(context: &StackContext) -> Result<StackValidation> {
    let mut validation = validate_stack(context)?;
    match ResolvedStack::from_context(context).await {
        Ok(resolved) => validate_resolved_stack(context, &resolved, &mut validation),
        Err(error) => validation.errors.push(error.to_string()),
    }
    validation.valid = validation.errors.is_empty();
    Ok(validation)
}

async fn resolve_validated_stack(
    context: &StackContext,
) -> Result<(StackValidation, ResolvedStack)> {
    let mut validation = validate_stack(context)?;
    let resolved = ResolvedStack::from_context(context).await?;
    validate_resolved_stack(context, &resolved, &mut validation);
    validation.valid = validation.errors.is_empty();
    Ok((validation, resolved))
}

pub(crate) async fn plan_stack<C>(
    client: &C,
    context: &StackContext,
    only_changes: bool,
    allow_secret_env: bool,
) -> Result<StackPlan>
where
    C: StackControlPlane + Sync,
{
    build_plan(client, context, only_changes, allow_secret_env).await
}

pub(crate) async fn apply_stack<C>(
    client: &C,
    context: StackContext,
    options: StackApplyOptions,
) -> Result<StackApplyReport>
where
    C: StackControlPlane + Sync,
{
    validate_self_contained_api_manifest(&context)?;
    let ledger_path = resolve_ledger_path(client, context.state_root_override.as_deref()).await?;
    let _ledger_lock = if options.dry_run {
        None
    } else {
        Some(LedgerLock::acquire(&ledger_path)?)
    };
    let mut plan = build_plan(client, &context, false, options.allow_secret_env).await?;
    let blocked_code = "stack_apply_blocked";
    let ledger_for_plan = ApplyLedger::load_or_new(&ledger_path).await?;
    let resolved = ResolvedStack::from_context(&context).await?;
    if options.force_restart && plan.restart_required {
        plan.warnings.push(
            "--force-restart was requested, but the daemon Stack API cannot restart or reload startup-only config; startup drift remains blocked".to_string(),
        );
    }
    if options.prune || context.document.spec.apply.prune {
        let prune_actions = plan_prune(&context, &resolved, &ledger_for_plan);
        plan.actions.extend(prune_actions);
        plan.summary = StackPlanSummary::from_actions(&plan.actions);
        plan.valid = plan.errors.is_empty();
    }
    if options.dry_run {
        return Ok(StackApplyReport::dry_run(plan));
    }
    if !plan.errors.is_empty() {
        return Err(crate::problems::DaemonProblem::unprocessable(
            "stacks",
            blocked_code,
            "KheishStack apply refused because the plan contains errors",
        )
        .into());
    }

    let mut ledger = ApplyLedger::load_or_new(&ledger_path).await?;
    ledger.save(&ledger_path).await?;
    let mut report = StackApplyReport {
        stack: context.document.metadata.name.clone(),
        ownership_id: context.ownership_id(),
        ledger_path: ledger_path.display().to_string(),
        applied: Vec::new(),
        verification: None,
        warnings: plan.warnings.clone(),
        plan: Some(plan.clone()),
    };
    enforce_desired_resource_ownership(&ledger, &context.ownership_id(), &resolved)?;

    apply_secrets(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        options.allow_secret_env,
        &mut report,
    )
    .await?;
    apply_runtime(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;
    apply_personas(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;
    apply_connectors(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;
    apply_sessions(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;
    apply_playbooks(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;
    apply_schedules(
        client,
        &context,
        &resolved,
        &ledger_path,
        &mut ledger,
        &mut report,
    )
    .await?;

    if options.prune || context.document.spec.apply.prune {
        let prune_actions = plan_prune(&context, &resolved, &ledger);
        execute_down(client, &context, &prune_actions, &mut ledger, &ledger_path).await?;
        report.applied.extend(
            prune_actions
                .into_iter()
                .filter(|action| action.operation != "blocked"),
        );
    }

    let verification = verify_stack(client, &context).await?;
    let verification_valid = verification.valid;
    report.verification = Some(verification);
    if verification_valid {
        Ok(report)
    } else {
        bail!("KheishStack apply completed but verification failed")
    }
}

pub(crate) async fn import_stack<C>(
    client: &C,
    context: StackContext,
    options: StackImportOptions,
) -> Result<StackImportReport>
where
    C: StackControlPlane + Sync,
{
    if options.resources.len() > STACK_MAX_RESOURCE_COUNT {
        return Err(crate::problems::DaemonProblem::unprocessable(
            "stacks",
            "stack_import_blocked",
            format!(
                "KheishStack import declares {} explicit resources, exceeding the {STACK_MAX_RESOURCE_COUNT} resource limit",
                options.resources.len()
            ),
        )
        .into());
    }
    let validation = validate_stack_context(&context).await?;
    if !validation.valid {
        return Err(crate::problems::DaemonProblem::unprocessable(
            "stacks",
            "stack_import_blocked",
            format!(
                "KheishStack import refused because validation failed: {}",
                validation.errors.join("; ")
            ),
        )
        .into());
    }
    let resolved = ResolvedStack::from_context(&context).await?;
    let declared_resources = resolved.desired_resource_keys();
    let resources = import_resources_from_manifest(&declared_resources, options.resources)?;
    let ledger_path = resolve_ledger_path(client, context.state_root_override.as_deref()).await?;
    let _ledger_lock = LedgerLock::acquire(&ledger_path)?;
    let mut ledger = ApplyLedger::load_or_new(&ledger_path).await?;
    let mut adopted = Vec::new();
    for resource in resources {
        let key = ResourceKey::parse(&resource)?;
        let live = fetch_live_resource_value(client, &key).await?;
        let digest = import_resource_digest(
            &key,
            &live,
            &resolved,
            &ledger,
            &context.ownership_id(),
            options.allow_secret_env,
        )?;
        if let Some(owner) = ledger.owner_of_resource(&key)
            && owner != context.ownership_id()
        {
            return Err(crate::problems::DaemonProblem::conflict(
                "stacks",
                "stack_ownership_conflict",
                format!("resource {key} is already owned by stack `{owner}`"),
            )
            .into());
        }
        if key.kind == "secret" {
            if let Some(fingerprint) =
                imported_secret_fingerprint(&key, &resolved, &ledger, options.allow_secret_env)?
            {
                ledger.record_secret(&context.ownership_id(), &key.id, fingerprint.clone());
                ledger.record_resource(&context.ownership_id(), &key, fingerprint);
            } else {
                ledger.record_resource(&context.ownership_id(), &key, digest);
            }
        } else {
            ledger.record_resource(&context.ownership_id(), &key, digest);
        }
        adopted.push(StackAction::new(
            "import",
            key.kind,
            key.id,
            "adopt",
            "resource exists and is now owned by this stack ledger",
        ));
    }
    ledger.save(&ledger_path).await?;
    Ok(StackImportReport {
        stack: context.document.metadata.name.clone(),
        ownership_id: context.ownership_id(),
        ledger_path: ledger_path.display().to_string(),
        adopted,
    })
}

fn import_resource_digest(
    key: &ResourceKey,
    live: &Value,
    resolved: &ResolvedStack,
    ledger: &ApplyLedger,
    ownership_id: &str,
    allow_secret_env: bool,
) -> Result<String> {
    if key.kind == "secret"
        && let Some(fingerprint) =
            imported_secret_fingerprint(key, resolved, ledger, allow_secret_env)?
    {
        return Ok(fingerprint);
    }
    if let Some(fingerprint) = ledger.secret_fingerprint(ownership_id, &key.id) {
        return Ok(fingerprint.to_string());
    }
    digest_json(live)
}

fn imported_secret_fingerprint(
    key: &ResourceKey,
    resolved: &ResolvedStack,
    ledger: &ApplyLedger,
    allow_secret_env: bool,
) -> Result<Option<String>> {
    if key.kind != "secret" {
        return Ok(None);
    }
    let Some(secret) = resolved.secrets.iter().find(|secret| secret.slot == key.id) else {
        return Ok(None);
    };
    let Some(env_name) = secret.value_env.as_deref() else {
        return Ok(None);
    };
    if !allow_secret_env {
        return Ok(None);
    }
    Ok(Some(secret_fingerprint(
        &ledger.ledger_salt,
        &secret.slot,
        env_name,
    )?))
}

fn import_resources_from_manifest(
    declared_resources: &[String],
    explicit_resources: Vec<String>,
) -> Result<Vec<String>> {
    if explicit_resources.is_empty() {
        return Ok(declared_resources.to_vec());
    }
    let declared = declared_resources.iter().collect::<BTreeSet<_>>();
    let mut resources = Vec::new();
    for resource in explicit_resources {
        let key = ResourceKey::parse(&resource).map_err(|error| {
            crate::problems::DaemonProblem::unprocessable(
                "stacks",
                "stack_import_blocked",
                format!("invalid stack import resource `{resource}`: {error}"),
            )
        })?;
        let normalized = key.to_string();
        if !declared.contains(&normalized) {
            return Err(crate::problems::DaemonProblem::unprocessable(
                "stacks",
                "stack_import_blocked",
                format!(
                    "stack import resource `{normalized}` is not declared in this KheishStack manifest"
                ),
            )
            .into());
        }
        resources.push(normalized);
    }
    Ok(resources)
}

pub(crate) async fn down_stack<C>(
    client: &C,
    context: StackContext,
    options: StackDownOptions,
) -> Result<StackDownReport>
where
    C: StackControlPlane + Sync,
{
    validate_self_contained_api_manifest(&context)?;
    let ledger_path = resolve_ledger_path(client, context.state_root_override.as_deref()).await?;
    let _ledger_lock = LedgerLock::acquire(&ledger_path)?;
    let mut ledger = ApplyLedger::load_or_new(&ledger_path).await?;
    let actions = plan_down(&context, &ledger);
    if options.yes {
        execute_down(client, &context, &actions, &mut ledger, &ledger_path).await?;
    }
    Ok(StackDownReport {
        stack: context.document.metadata.name.clone(),
        ownership_id: context.ownership_id(),
        ledger_path: ledger_path.display().to_string(),
        executed: options.yes,
        actions,
    })
}

async fn build_plan<C>(
    client: &C,
    context: &StackContext,
    only_changes: bool,
    allow_secret_env: bool,
) -> Result<StackPlan>
where
    C: StackControlPlane + Sync,
{
    let ledger_path = resolve_ledger_path(client, context.state_root_override.as_deref()).await?;
    let ledger = ApplyLedger::load_or_new(&ledger_path).await?;
    let (mut validation, resolved) = resolve_validated_stack(context).await?;
    let mut plan = StackPlan {
        stack: context.document.metadata.name.clone(),
        ownership_id: context.ownership_id(),
        ledger_path: ledger_path.display().to_string(),
        valid: validation.valid,
        restart_required: false,
        actions: Vec::new(),
        errors: std::mem::take(&mut validation.errors),
        warnings: std::mem::take(&mut validation.warnings),
        summary: StackPlanSummary::default(),
    };

    add_startup_actions(context, &resolved, &mut plan);
    add_mcp_requirement_actions(client, &resolved, &mut plan).await?;
    add_secret_actions(
        client,
        context,
        &resolved,
        &ledger,
        allow_secret_env,
        &mut plan,
    )
    .await?;
    add_runtime_actions(client, context, &resolved, &ledger, &mut plan).await?;
    add_persona_actions(client, &resolved, &mut plan).await?;
    add_connector_actions(client, context, &resolved, &ledger, &mut plan).await?;
    add_session_actions(client, &resolved, &mut plan).await?;
    add_playbook_actions(client, &resolved, &mut plan).await?;
    add_schedule_actions(client, context, &resolved, &ledger, &mut plan).await?;
    add_verification_actions(&resolved, &mut plan);
    add_ownership_diagnostics(&ledger, &context.ownership_id(), &mut plan);
    plan.summary = StackPlanSummary::from_actions(&plan.actions);
    if only_changes {
        plan.actions
            .retain(|action| action.operation != "noop" && action.operation != "verify");
        plan.summary = StackPlanSummary::from_actions(&plan.actions);
    }
    plan.valid = plan.errors.is_empty();
    Ok(plan)
}

fn validate_self_contained_api_manifest(context: &StackContext) -> Result<()> {
    if context.allow_file_refs {
        return Ok(());
    }
    let mut file_refs = Vec::new();
    for persona in &context.document.spec.personas {
        if persona.soul_file.is_some() {
            file_refs.push(format!("spec.personas[{}].soul_file", persona.persona_id));
        }
    }
    for schedule in &context.document.spec.schedules {
        if schedule
            .request
            .as_ref()
            .and_then(|request| request.content_file.as_ref())
            .is_some()
        {
            file_refs.push(format!(
                "spec.schedules[{}].request.content_file",
                schedule.name
            ));
        }
        if schedule
            .flow_start
            .as_ref()
            .and_then(|flow_start| flow_start.request.content_file.as_ref())
            .is_some()
        {
            file_refs.push(format!(
                "spec.schedules[{}].flow_start.request.content_file",
                schedule.name
            ));
        }
    }
    for (index, playbook) in context.document.spec.playbooks.iter().enumerate() {
        if playbook.manifest_file.is_some() {
            file_refs.push(format!("spec.playbooks[{index}].manifest_file"));
        }
    }
    if !file_refs.is_empty() {
        bail!(
            "file references are not allowed through the daemon Stack API; submit a self-contained manifest: {}",
            file_refs.join(", ")
        );
    }
    Ok(())
}

fn validate_stack(context: &StackContext) -> Result<StackValidation> {
    let mut validation = StackValidation {
        stack: context.document.metadata.name.clone(),
        valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    let document = &context.document;
    if document.api_version != STACK_API_VERSION {
        validation.errors.push(format!(
            "apiVersion must be {STACK_API_VERSION}, got {}",
            document.api_version
        ));
    }
    if document.kind != STACK_KIND {
        validation
            .errors
            .push(format!("kind must be {STACK_KIND}, got {}", document.kind));
    }
    if document.metadata.name.trim().is_empty() {
        validation
            .errors
            .push("metadata.name is required".to_string());
    }
    for (key, value) in &document.metadata.labels {
        if key.trim().is_empty() || value.trim().is_empty() {
            validation
                .errors
                .push("metadata.labels cannot contain empty keys or values".to_string());
        }
    }
    validate_resource_count(document, &mut validation);
    validate_unique(
        "spec.personas[].persona_id",
        document
            .spec
            .personas
            .iter()
            .map(|persona| persona.persona_id.as_str()),
        &mut validation,
    );
    validate_unique(
        "spec.sessions[].session_id",
        document
            .spec
            .sessions
            .iter()
            .map(|session| session.session_id.as_str()),
        &mut validation,
    );
    validate_unique(
        "spec.schedules[].name",
        document
            .spec
            .schedules
            .iter()
            .map(|schedule| schedule.name.as_str()),
        &mut validation,
    );
    validate_unique(
        "spec.requires.mcp.servers",
        document
            .spec
            .requires
            .mcp
            .servers
            .iter()
            .map(StackMcpServerRequirementSpec::name),
        &mut validation,
    );
    validate_unique(
        "spec.requires.mcp.tools",
        document.spec.requires.mcp.tools.iter().map(String::as_str),
        &mut validation,
    );
    validate_session_operator_configs(document, &mut validation);
    validate_mcp_requirement_details(&document.spec.requires.mcp, &mut validation);
    validate_unique(
        "spec.playbooks[] playbook_id/version",
        document.spec.playbooks.iter().filter_map(|playbook| {
            playbook
                .manifest
                .as_ref()
                .map(|manifest| format!("{}/{}", manifest.playbook_id, manifest.version))
        }),
        &mut validation,
    );
    if !document.spec.agent_templates.is_empty() {
        validation.errors.push(
            "spec.agent_templates is not supported by KheishStack v1alpha1; use built-in profiles or add a daemon store/API first".to_string(),
        );
    }
    for connector in &document.spec.connectors {
        if connector.kind != "http" {
            validation.errors.push(format!(
                "spec.connectors[{}] uses kind `{}`; KheishStack v1alpha1 supports only kind `http` until every connector kind has live drift comparison",
                connector.name, connector.kind
            ));
        } else if let Err(error) = parse_http_connector_request(&connector.name, &connector.spec) {
            validation.errors.push(format!(
                "spec.connectors[{}].spec is not a valid http connector payload: {error}",
                connector.name
            ));
        }
    }
    if !document.spec.verification.is_empty() {
        validation.warnings.push(
            "spec.verification currently supports existence probes only; runtime tool-deny probes require a future probe runner".to_string(),
        );
    }
    if context.strict_scopes() {
        validate_strict_scopes(context, &mut validation);
    }
    validation.valid = validation.errors.is_empty();
    Ok(validation)
}

fn validate_session_operator_configs(document: &StackDocument, validation: &mut StackValidation) {
    for session in &document.spec.sessions {
        let Some(operator) = session.operator.as_ref() else {
            continue;
        };
        if let Err(error) =
            crate::operator_contact::normalize_session_operator_config(operator.clone())
        {
            validation.errors.push(format!(
                "spec.sessions[{}].operator is invalid: {error}",
                session.session_id
            ));
            continue;
        }
        if !operator.enabled {
            continue;
        }
        if !operator.allow_notify && !operator.allow_questions {
            validation.errors.push(format!(
                "spec.sessions[{}].operator must allow notify_operator or ask_operator when enabled",
                session.session_id
            ));
        }
        if operator.allow_notify && session.reply_targets.as_ref().is_none_or(Vec::is_empty) {
            validation.errors.push(format!(
                "spec.sessions[{}].operator.allow_notify requires at least one reply_targets entry",
                session.session_id
            ));
        }
    }
}

fn validate_resource_count(document: &StackDocument, validation: &mut StackValidation) {
    let resource_count = document.spec.requires.secrets.len()
        + document.spec.requires.mcp.servers.len()
        + document.spec.requires.mcp.tools.len()
        + usize::from(document.spec.startup.routes.is_some())
        + usize::from(document.spec.startup.connectors_config.is_some())
        + document.spec.startup.mcp_profiles.len()
        + usize::from(document.spec.runtime.permission_mode.is_some())
        + usize::from(document.spec.runtime.debug_level.is_some())
        + document.spec.connectors.len()
        + document.spec.personas.len()
        + document.spec.sessions.len()
        + document.spec.schedules.len()
        + document.spec.playbooks.len()
        + document.spec.verification.len()
        + document.spec.agent_templates.len();
    if resource_count > STACK_MAX_RESOURCE_COUNT {
        validation.errors.push(format!(
            "KheishStack declares {resource_count} resources, exceeding the {STACK_MAX_RESOURCE_COUNT} resource limit"
        ));
    }
}

fn validate_unique<'a, I, S>(path: &str, values: I, validation: &mut StackValidation)
where
    I: IntoIterator<Item = S>,
    S: Into<std::borrow::Cow<'a, str>>,
{
    let mut seen = BTreeSet::new();
    for value in values {
        let value = value.into();
        if value.trim().is_empty() {
            validation.errors.push(format!("{path} cannot be empty"));
            continue;
        }
        if !seen.insert(value.to_string()) {
            validation
                .errors
                .push(format!("{path} contains duplicate value {value}"));
        }
    }
}

fn validate_mcp_requirement_details(
    requirements: &StackMcpRequirements,
    validation: &mut StackValidation,
) {
    for server in &requirements.servers {
        let StackMcpServerRequirementSpec::Detailed(details) = server else {
            continue;
        };
        if details
            .source
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            validation
                .errors
                .push("spec.requires.mcp.servers[].source cannot be empty".to_string());
        }
        if details
            .catalog_entry_id
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            validation
                .errors
                .push("spec.requires.mcp.servers[].catalog_entry_id cannot be empty".to_string());
        }
        validate_unique(
            "spec.requires.mcp.servers[].credential_secret_refs",
            details.credential_secret_refs.iter().map(String::as_str),
            validation,
        );
    }
}

fn parse_http_connector_request(
    name: &str,
    spec: &Value,
) -> Result<crate::PutHttpConnectorRequest> {
    validate_http_connector_spec_shape(name, spec)?;
    serde_json::from_value::<crate::PutHttpConnectorRequest>(spec.clone())
        .context("failed to decode http connector spec")
}

fn validate_http_connector_spec_shape(name: &str, spec: &Value) -> Result<()> {
    let path = format!("spec.connectors[{name}].spec");
    validate_object_fields(
        spec,
        &path,
        &[
            "actor_id",
            "fixed_session_id",
            "bearer_token",
            "hmac_secret",
            "allow_unauthenticated_ingress",
            "require_hmac_signature",
            "signature_max_age_secs",
            "require_idempotency_key",
            "ingress_events_per_second",
            "allow_payload_reply_targets",
            "default_reply_targets",
            "default_binding_keys",
            "session_policy",
        ],
    )?;
    let Some(object) = spec.as_object() else {
        return Ok(());
    };
    for field in ["bearer_token", "hmac_secret"] {
        if let Some(value) = object.get(field) {
            validate_connector_secret_input_shape(value, &format!("{path}.{field}"))?;
        }
    }
    if let Some(value) = object.get("session_policy") {
        validate_connector_session_policy_shape(value, &format!("{path}.session_policy"))?;
    }
    Ok(())
}

fn validate_connector_secret_input_shape(value: &Value, path: &str) -> Result<()> {
    validate_object_fields(value, path, &["secret_ref", "value", "env"])?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{path} must be an object"))?;
    let configured = ["secret_ref", "value", "env"]
        .into_iter()
        .filter(|field| object.get(*field).is_some_and(|value| !value.is_null()))
        .count();
    if configured != 1 {
        bail!("{path} must set exactly one of secret_ref, value, or env");
    }
    if object.get("value").is_some_and(|value| !value.is_null()) {
        bail!(
            "{path}.value is not supported in KheishStack; use secret_ref or env so drift can be compared without reading secret values"
        );
    }
    Ok(())
}

fn validate_connector_session_policy_shape(value: &Value, path: &str) -> Result<()> {
    validate_object_fields(
        value,
        path,
        &[
            "create_if_missing",
            "persona_id",
            "capability_scope",
            "credential_scope",
        ],
    )?;
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if let Some(scope) = object.get("capability_scope") {
        validate_object_fields(
            scope,
            &format!("{path}.capability_scope"),
            &[
                "skill_allow",
                "skill_deny",
                "mcp_server_allow",
                "mcp_server_deny",
                "mcp_tool_allow",
                "mcp_tool_deny",
            ],
        )?;
    }
    if let Some(scope) = object.get("credential_scope") {
        validate_object_fields(
            scope,
            &format!("{path}.credential_scope"),
            &[
                "route_allow",
                "route_deny",
                "connector_allow",
                "connector_deny",
                "connector_credential_allow",
                "connector_credential_deny",
                "mcp_server_allow",
                "mcp_server_deny",
            ],
        )?;
    }
    Ok(())
}

fn validate_object_fields(value: &Value, path: &str, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{path} must be an object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!("{path}.{key} is not supported");
        }
    }
    Ok(())
}

fn validate_strict_scopes(context: &StackContext, validation: &mut StackValidation) {
    let allow_wildcards = context.document.spec.apply.allow_wildcard_scopes;
    for connector in &context.document.spec.connectors {
        validate_connector_session_policy(context, connector, allow_wildcards, validation);
    }
    for persona in &context.document.spec.personas {
        let path = format!("spec.personas[{}].capability_scope", persona.persona_id);
        validate_capability_scope(
            persona.capability_scope.as_ref(),
            &path,
            allow_wildcards,
            validation,
        );
    }
    for session in &context.document.spec.sessions {
        let capability_path = format!("spec.sessions[{}].capability_scope", session.session_id);
        validate_capability_scope(
            session.capability_scope.as_ref(),
            &capability_path,
            allow_wildcards,
            validation,
        );
        let credential_path = format!("spec.sessions[{}].credential_scope", session.session_id);
        validate_credential_scope(
            session.credential_scope.as_ref(),
            &credential_path,
            allow_wildcards,
            validation,
        );
        let session_provider = session
            .route_policy
            .as_ref()
            .and_then(|policy| policy.provider.as_deref());
        if let Some(provider) = session_provider
            && let Some(scope) = session.credential_scope.as_ref()
            && !scope.normalized().allows_route(provider)
        {
            validation.errors.push(format!(
                "spec.sessions[{}].route_policy.provider `{provider}` is not allowed by credential_scope.route_*",
                session.session_id
            ));
        }
        if let Some(persona_id) = session.persona_id.as_deref() {
            let Some(persona) = context
                .document
                .spec
                .personas
                .iter()
                .find(|candidate| candidate.persona_id == persona_id)
            else {
                validation.errors.push(format!(
                    "spec.sessions[{}].persona_id `{persona_id}` must reference a persona declared in this stack so strict capability intersections can be validated",
                    session.session_id
                ));
                continue;
            };
            if let (Some(persona_scope), Some(session_scope)) = (
                persona.capability_scope.as_ref(),
                session.capability_scope.as_ref(),
            ) {
                validate_capability_intersections(
                    persona_scope,
                    session_scope,
                    &format!(
                        "spec.sessions[{}] restricts persona {}",
                        session.session_id, persona_id
                    ),
                    validation,
                );
            }
        }
    }
    for schedule in &context.document.spec.schedules {
        let payload_count = [
            schedule.request.is_some(),
            schedule.observation_materialization.is_some(),
            schedule.flow_start.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if payload_count != 1 {
            validation.errors.push(format!(
                "spec.schedules[{}] must define exactly one of request, observation_materialization, or flow_start",
                schedule.name
            ));
        }
        let Some(session) = context
            .document
            .spec
            .sessions
            .iter()
            .find(|candidate| candidate.session_id == schedule.target_session_id)
        else {
            validation.errors.push(format!(
                "spec.schedules[{}].target_session_id `{}` must reference a session declared in this stack so scheduled runs cannot target external fail-open sessions",
                schedule.name, schedule.target_session_id
            ));
            continue;
        };
        if let Some(observation_materialization) = schedule.observation_materialization.as_ref()
            && observation_materialization.target_session_id != schedule.target_session_id
        {
            validation.errors.push(format!(
                "spec.schedules[{}].observation_materialization.target_session_id must match target_session_id `{}`",
                schedule.name, schedule.target_session_id
            ));
        }
        if let Some(flow_start) = schedule.flow_start.as_ref()
            && let Some(session_id) = flow_start.session_id.as_ref()
            && session_id != &schedule.target_session_id
        {
            validation.errors.push(format!(
                "spec.schedules[{}].flow_start.session_id must match target_session_id `{}`",
                schedule.name, schedule.target_session_id
            ));
        }
        if let Some(flow_start) = schedule.flow_start.as_ref()
            && flow_start
                .request
                .metadata
                .as_ref()
                .is_some_and(|metadata| !metadata.is_null() && !metadata.is_object())
        {
            validation.errors.push(format!(
                "spec.schedules[{}].flow_start.request.metadata must be an object",
                schedule.name
            ));
        }
        if let Some(flow_start) = schedule.flow_start.as_ref()
            && !matches!(schedule.cadence, crate::ScheduleCadence::Once { .. })
        {
            if flow_start
                .flow_id
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            {
                validation.errors.push(format!(
                    "spec.schedules[{}].flow_start.flow_id is only supported for one-shot schedules",
                    schedule.name
                ));
            }
            if flow_start
                .idempotency_key
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            {
                validation.errors.push(format!(
                    "spec.schedules[{}].flow_start.idempotency_key is only supported for one-shot schedules",
                    schedule.name
                ));
            }
        }
        if let Some(request) = schedule.request.as_ref() {
            validate_scheduled_request_route(
                &format!("spec.schedules[{}].request", schedule.name),
                request.provider.as_deref(),
                request.generation.as_ref(),
                session,
                validation,
            );
        }
        if let Some(observation) = schedule.observation_materialization.as_ref() {
            validate_scheduled_request_route(
                &format!(
                    "spec.schedules[{}].observation_materialization.request",
                    schedule.name
                ),
                observation.request.provider.as_deref(),
                observation.request.generation.as_ref(),
                session,
                validation,
            );
        }
    }
}

fn validate_resolved_stack(
    context: &StackContext,
    resolved: &ResolvedStack,
    validation: &mut StackValidation,
) {
    validate_resolved_flow_start_playbooks(resolved, validation);
    if context.strict_scopes() {
        validate_resolved_flow_start_routes(resolved, validation);
    }
}

fn validate_resolved_flow_start_playbooks(
    resolved: &ResolvedStack,
    validation: &mut StackValidation,
) {
    for schedule in &resolved.schedules {
        let Some(flow_start) = schedule.request.flow_start.as_ref() else {
            continue;
        };
        let Some(playbook) = resolved_playbook_for_ref(resolved, &flow_start.playbook_ref) else {
            continue;
        };
        let status = playbook
            .publish
            .as_ref()
            .map(|publish| publish.status.clone())
            .unwrap_or_default();
        if !status.is_startable() {
            validation.warnings.push(format!(
                "spec.schedules[{}].flow_start references same-stack playbook {}@{} with non-startable release status `{status:?}`; the schedule can be created but Flow starts will fail until the playbook is published as verified, canary, or active",
                schedule.name,
                flow_start.playbook_ref.playbook_id,
                flow_start.playbook_ref.version
            ));
        }
    }
}

fn validate_resolved_flow_start_routes(resolved: &ResolvedStack, validation: &mut StackValidation) {
    for schedule in &resolved.schedules {
        let Some(flow_start) = schedule.request.flow_start.as_ref() else {
            continue;
        };
        let Some(session) = resolved
            .sessions
            .iter()
            .find(|session| session.session_id == schedule.request.target_session_id)
        else {
            continue;
        };
        let playbook = resolved_playbook_for_ref(resolved, &flow_start.playbook_ref);
        validate_scheduled_flow_start_route(
            &format!("spec.schedules[{}].flow_start.request", schedule.name),
            flow_start,
            session,
            playbook,
            validation,
        );
    }
}

fn resolved_playbook_for_ref<'a>(
    resolved: &'a ResolvedStack,
    reference: &crate::PlaybookVersionRef,
) -> Option<&'a ResolvedPlaybook> {
    resolved.playbooks.iter().find(|playbook| {
        playbook.manifest.playbook_id == reference.playbook_id
            && playbook.manifest.version == reference.version
            && playbook.digest == reference.digest
    })
}

fn validate_scheduled_flow_start_route(
    path: &str,
    flow_start: &crate::StartFlowRequest,
    session: &ResolvedSession,
    playbook: Option<&ResolvedPlaybook>,
    validation: &mut StackValidation,
) {
    let request_provider = flow_start.request.provider.as_deref();
    let default_provider =
        playbook.and_then(|playbook| playbook.manifest.runtime_defaults.provider.as_deref());
    let default_model =
        playbook.and_then(|playbook| playbook.manifest.runtime_defaults.model.as_deref());
    let request_has_model = flow_start
        .request
        .generation
        .as_ref()
        .and_then(|generation| generation.model.as_ref())
        .is_some();
    let default_model_applies = !request_has_model
        && default_model.is_some()
        && request_provider.zip(default_provider).is_none_or(
            |(request_provider, default_provider)| request_provider == default_provider,
        );
    let has_model = request_has_model || default_model_applies;
    let provider = request_provider.or(default_provider).or_else(|| {
        (!has_model)
            .then(|| {
                session
                    .route_policy
                    .as_ref()
                    .and_then(|policy| policy.provider.as_deref())
            })
            .flatten()
    });
    validate_route_provider_scope(
        path,
        provider,
        session.credential_scope.as_ref(),
        validation,
    );
}

fn validate_scheduled_request_route(
    path: &str,
    provider: Option<&str>,
    generation: Option<&kheish_runtime::ModelGenerationConfig>,
    session: &StackSessionSpec,
    validation: &mut StackValidation,
) {
    let provider = provider.or_else(|| {
        generation
            .and_then(|generation| generation.model.as_deref())
            .is_none()
            .then(|| {
                session
                    .route_policy
                    .as_ref()
                    .and_then(|policy| policy.provider.as_deref())
            })
            .flatten()
    });
    validate_route_provider_scope(
        path,
        provider,
        session.credential_scope.as_ref(),
        validation,
    );
}

fn validate_route_provider_scope(
    path: &str,
    provider: Option<&str>,
    scope: Option<&kheish_types::CredentialScope>,
    validation: &mut StackValidation,
) {
    let Some(scope) = scope else {
        return;
    };
    let Some(provider) = provider else {
        return;
    };
    if !scope.normalized().allows_route(provider) {
        validation.errors.push(format!(
            "{path}.provider `{provider}` is not allowed by target session credential_scope.route_*"
        ));
    }
}

fn validate_connector_session_policy(
    context: &StackContext,
    connector: &StackConnectorSpec,
    allow_wildcards: bool,
    validation: &mut StackValidation,
) {
    if connector.kind != "http" {
        return;
    }
    let Ok(request) = parse_http_connector_request(&connector.name, &connector.spec) else {
        return;
    };
    if let Some(fixed_session_id) = request.fixed_session_id.as_deref()
        && !context
            .document
            .spec
            .sessions
            .iter()
            .any(|session| session.session_id == fixed_session_id)
    {
        validation.errors.push(format!(
            "spec.connectors[{}].spec.fixed_session_id `{fixed_session_id}` must reference a session declared in this stack",
            connector.name
        ));
    }
    let Some(policy) = request.session_policy else {
        if request.fixed_session_id.is_none() {
            validation.errors.push(format!(
                "spec.connectors[{}].spec must set fixed_session_id or a fail-closed session_policy",
                connector.name
            ));
        }
        return;
    };
    if policy.is_empty() {
        if request.fixed_session_id.is_none() {
            validation.errors.push(format!(
                "spec.connectors[{}].spec.session_policy must be non-empty when fixed_session_id is omitted",
                connector.name
            ));
        }
        return;
    }
    let policy = policy.normalized();
    let capability_path = format!(
        "spec.connectors[{}].spec.session_policy.capability_scope",
        connector.name
    );
    validate_capability_scope(
        Some(&policy.capability_scope),
        &capability_path,
        allow_wildcards,
        validation,
    );
    let credential_path = format!(
        "spec.connectors[{}].spec.session_policy.credential_scope",
        connector.name
    );
    validate_credential_scope(
        Some(&policy.credential_scope),
        &credential_path,
        allow_wildcards,
        validation,
    );
    if let Some(persona_id) = policy.persona_id.as_deref() {
        let Some(persona) = context
            .document
            .spec
            .personas
            .iter()
            .find(|candidate| candidate.persona_id == persona_id)
        else {
            validation.errors.push(format!(
                "spec.connectors[{}].spec.session_policy.persona_id `{persona_id}` must reference a persona declared in this stack so strict capability intersections can be validated",
                connector.name
            ));
            return;
        };
        if let Some(persona_scope) = persona.capability_scope.as_ref() {
            validate_capability_intersections(
                persona_scope,
                &policy.capability_scope,
                &format!(
                    "spec.connectors[{}].spec.session_policy restricts persona {}",
                    connector.name, persona_id
                ),
                validation,
            );
        }
    }
}

fn validate_capability_scope(
    scope: Option<&kheish_types::CapabilityScope>,
    path: &str,
    allow_wildcards: bool,
    validation: &mut StackValidation,
) {
    let Some(scope) = scope else {
        validation.errors.push(format!(
            "{path} is required because omitted capability scopes are fail-open"
        ));
        return;
    };
    let scope = scope.normalized();
    validate_scope_family(
        &scope.skill_allow,
        &scope.skill_deny,
        &format!("{path}.skill"),
        allow_wildcards,
        validation,
    );
    validate_scope_family(
        &scope.mcp_server_allow,
        &scope.mcp_server_deny,
        &format!("{path}.mcp_server"),
        allow_wildcards,
        validation,
    );
    validate_scope_family(
        &scope.mcp_tool_allow,
        &scope.mcp_tool_deny,
        &format!("{path}.mcp_tool"),
        allow_wildcards,
        validation,
    );
}

fn validate_credential_scope(
    scope: Option<&kheish_types::CredentialScope>,
    path: &str,
    allow_wildcards: bool,
    validation: &mut StackValidation,
) {
    let Some(scope) = scope else {
        validation.errors.push(format!(
            "{path} is required because omitted credential scopes are fail-open"
        ));
        return;
    };
    let scope = scope.normalized();
    validate_scope_family(
        &scope.route_allow,
        &scope.route_deny,
        &format!("{path}.route"),
        allow_wildcards,
        validation,
    );
    validate_scope_family(
        &scope.connector_allow,
        &scope.connector_deny,
        &format!("{path}.connector"),
        allow_wildcards,
        validation,
    );
    validate_scope_family(
        &scope.connector_credential_allow,
        &scope.connector_credential_deny,
        &format!("{path}.connector_credential"),
        allow_wildcards,
        validation,
    );
    validate_scope_family(
        &scope.mcp_server_allow,
        &scope.mcp_server_deny,
        &format!("{path}.mcp_server"),
        allow_wildcards,
        validation,
    );
}

fn validate_scope_family(
    allow: &[String],
    deny: &[String],
    path: &str,
    allow_wildcards: bool,
    validation: &mut StackValidation,
) {
    if allow.is_empty() && deny.is_empty() {
        validation.errors.push(format!(
            "{path} must set an allow or deny list; empty lists are fail-open in the daemon"
        ));
    }
    if allow.is_empty() && !deny.is_empty() && !deny.iter().any(|entry| entry == "*") {
        validation.errors.push(format!(
            "{path}_deny must contain `*` when {path}_allow is empty; partial deny-lists are still fail-open in the daemon"
        ));
    }
    if !allow_wildcards && allow.iter().any(|entry| entry == "*") {
        validation.errors.push(format!(
            "{path}_allow uses `*`; set spec.apply.allow_wildcard_scopes=true only for intentional allow-all"
        ));
    }
}

fn validate_capability_intersections(
    persona_scope: &kheish_types::CapabilityScope,
    session_scope: &kheish_types::CapabilityScope,
    path: &str,
    validation: &mut StackValidation,
) {
    validate_allow_intersection(
        &persona_scope.normalized().skill_allow,
        &session_scope.normalized().skill_allow,
        &format!("{path}.skill_allow"),
        validation,
    );
    validate_allow_intersection(
        &persona_scope.normalized().mcp_server_allow,
        &session_scope.normalized().mcp_server_allow,
        &format!("{path}.mcp_server_allow"),
        validation,
    );
    validate_allow_intersection(
        &persona_scope.normalized().mcp_tool_allow,
        &session_scope.normalized().mcp_tool_allow,
        &format!("{path}.mcp_tool_allow"),
        validation,
    );
}

fn validate_allow_intersection(
    left: &[String],
    right: &[String],
    path: &str,
    validation: &mut StackValidation,
) {
    if left.is_empty()
        || right.is_empty()
        || left.iter().any(|entry| entry == "*")
        || right.iter().any(|entry| entry == "*")
    {
        return;
    }
    let right = right.iter().collect::<BTreeSet<_>>();
    if !left.iter().any(|entry| right.contains(entry)) {
        validation.errors.push(format!(
            "{path} has disjoint allow-lists; daemon restrict_with would collapse to an empty allow-list that becomes fail-open"
        ));
    }
}

async fn resolve_ledger_path<C>(client: &C, state_root_override: Option<&Path>) -> Result<PathBuf>
where
    C: StackControlPlane + Sync,
{
    if let Some(state_root) = state_root_override {
        return Ok(state_root.join(LEDGER_DIR).join(LEDGER_FILE));
    }
    let runtime = client
        .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
        .await?;
    if let Some(state_root) = runtime.state_root {
        return Ok(PathBuf::from(state_root).join(LEDGER_DIR).join(LEDGER_FILE));
    }
    if let Some(store_path) = runtime.config.store_path
        && let Some(parent) = Path::new(&store_path).parent()
    {
        return Ok(parent.join(LEDGER_DIR).join(LEDGER_FILE));
    }
    Ok(PathBuf::from(".kheish-daemon")
        .join(LEDGER_DIR)
        .join(LEDGER_FILE))
}

fn add_startup_actions(context: &StackContext, resolved: &ResolvedStack, plan: &mut StackPlan) {
    if resolved.startup_digest.is_none() {
        return;
    }
    plan.restart_required = true;
    let _restart_policy = context.document.spec.apply.restart_policy;
    plan.errors.push(
        "startup-only configuration is loaded at serve time and cannot be reconciled by the daemon Stack API; restart the daemon with that config outside apply".to_string(),
    );
    plan.actions.push(StackAction::new(
        "startup",
        "daemon",
        "serve_config",
        "blocked",
        "startup-only config is not hot-reloaded by the daemon",
    ));
}

async fn add_mcp_requirement_actions<C>(
    client: &C,
    resolved: &ResolvedStack,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    if resolved.mcp_requirements.servers.is_empty() && resolved.mcp_requirements.tools.is_empty() {
        return Ok(());
    }
    let runtime = client
        .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
        .await?;
    for server in &resolved.mcp_requirements.servers {
        let (present, reason) = mcp_server_requirement_status(&runtime, server);
        if !present {
            plan.errors.push(format!(
                "required MCP server `{}` is not satisfied in the daemon runtime: {reason}",
                server.name
            ));
        }
        plan.actions.push(StackAction::new(
            "requirements",
            "mcp_server",
            &server.name,
            if present { "noop" } else { "blocked" },
            reason,
        ));
    }
    for tool in &resolved.mcp_requirements.tools {
        let present = mcp_tool_active(&runtime, tool);
        if !present {
            plan.errors.push(format!(
                "required MCP tool `{tool}` is not active in the daemon runtime"
            ));
        }
        plan.actions.push(StackAction::new(
            "requirements",
            "mcp_tool",
            tool,
            if present { "noop" } else { "blocked" },
            if present {
                "required MCP tool is active"
            } else {
                "required MCP tool is missing from the active MCP surface"
            },
        ));
    }
    Ok(())
}

fn mcp_server_requirement_status(
    runtime: &crate::RuntimeSettingsView,
    requirement: &ResolvedMcpServerRequirement,
) -> (bool, String) {
    let Some(server) = runtime
        .mcp
        .servers
        .iter()
        .find(|server| server.server == requirement.name)
    else {
        return (false, "required MCP server is absent".to_string());
    };
    if !server.connected {
        return (false, "required MCP server is disconnected".to_string());
    }
    if let Some(source) = requirement.source.as_deref()
        && server.source.as_deref() != Some(source)
    {
        return (
            false,
            format!(
                "required MCP server source `{source}` does not match live source `{}`",
                server.source.as_deref().unwrap_or("<none>")
            ),
        );
    }
    if let Some(catalog_entry_id) = requirement.catalog_entry_id.as_deref()
        && server.catalog_entry_id.as_deref() != Some(catalog_entry_id)
    {
        return (
            false,
            format!(
                "required MCP server catalog entry `{catalog_entry_id}` does not match live catalog entry `{}`",
                server.catalog_entry_id.as_deref().unwrap_or("<none>")
            ),
        );
    }
    if let Some(uses_credentials) = requirement.uses_credentials
        && server.uses_credentials != uses_credentials
    {
        return (
            false,
            format!(
                "required MCP server uses_credentials={uses_credentials} does not match live uses_credentials={}",
                server.uses_credentials
            ),
        );
    }
    for secret_ref in &requirement.credential_secret_refs {
        if !server
            .credential_secret_refs
            .iter()
            .any(|live| live == secret_ref)
        {
            return (
                false,
                format!("required MCP server does not reference secret `{secret_ref}`"),
            );
        }
    }
    (true, "required MCP server is connected".to_string())
}

fn mcp_tool_active(runtime: &crate::RuntimeSettingsView, name: &str) -> bool {
    runtime.mcp.tool_names.iter().any(|tool| tool == name)
}

async fn add_secret_actions<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger: &ApplyLedger,
    allow_secret_env: bool,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    let ownership_id = context.ownership_id();
    for secret in &resolved.secrets {
        let encoded = url_encode_path_segment(&secret.slot);
        let live = client
            .get_json_optional::<kheish_auth::AuthSlotStatus>(&format!(
                "/v1/runtime/secrets/{encoded}"
            ))
            .await?;
        let key = ResourceKey::new("secret", &secret.slot);
        let desired_fingerprint = if let Some(env_name) = secret.value_env.as_deref() {
            if allow_secret_env {
                match secret_fingerprint(&ledger.ledger_salt, &secret.slot, env_name) {
                    Ok(fingerprint) => Some(fingerprint),
                    Err(error) => {
                        plan.errors.push(error.to_string());
                        None
                    }
                }
            } else {
                plan.errors.push(format!(
                    "secret {} uses value_env={env_name}; pass allow_secret_env=true to read the daemon environment for planning/apply",
                    secret.slot
                ));
                None
            }
        } else {
            None
        };
        let provider_mismatch = live
            .as_ref()
            .is_some_and(|status| status.provider != secret.provider);
        let operation = if live.is_none() {
            if secret.value_env.is_some() {
                "create"
            } else {
                plan.errors.push(format!(
                    "required secret {} is missing and has no value_env source",
                    secret.slot
                ));
                "blocked"
            }
        } else if provider_mismatch {
            if secret.value_env.is_some() {
                "update"
            } else {
                let live_provider = live
                    .as_ref()
                    .map(|status| status.provider.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                plan.errors.push(format!(
                    "required secret {} exists with provider {live_provider} but manifest requires {} and has no value_env source",
                    secret.slot,
                    secret.provider
                ));
                "blocked"
            }
        } else if secret.value_env.is_none() {
            "verify"
        } else if let Some(fingerprint) = desired_fingerprint.as_deref() {
            if ledger
                .secret_fingerprint(&ownership_id, &secret.slot)
                .is_some_and(|stored| stored == fingerprint)
                && live.as_ref().is_some_and(|status| {
                    !managed_secret_modified_after_apply(ledger, &ownership_id, &key, status)
                })
            {
                "noop"
            } else {
                "update"
            }
        } else {
            "noop"
        };
        plan.actions.push(StackAction::new(
            "secrets",
            key.kind,
            key.id,
            operation,
            if secret.value_env.is_some() {
                "secret values are not read back; drift is tracked by ledger fingerprints when value_env is provided"
            } else {
                "required secret exists and provider matches; unmanaged prerequisite secrets are not owned by the stack ledger"
            },
        ));
    }
    Ok(())
}

async fn add_runtime_actions<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger: &ApplyLedger,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    if resolved.runtime_digest.is_none() {
        return Ok(());
    }
    let runtime = client
        .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
        .await?;
    if let Some(permission_mode) = resolved.runtime.permission_mode.as_ref() {
        let operation = if runtime.permission_mode == *permission_mode {
            "noop"
        } else {
            "update"
        };
        plan.actions.push(StackAction::new(
            "runtime",
            "runtime",
            "permission_mode",
            operation,
            "runtime singleton uses daemon revision compare-and-swap on apply",
        ));
    }
    if let Some(debug_level) = resolved.runtime.debug_level {
        let operation = if runtime.debug_level == debug_level {
            "noop"
        } else {
            "update"
        };
        plan.actions.push(StackAction::new(
            "runtime",
            "runtime",
            "debug_level",
            operation,
            "runtime singleton uses daemon revision compare-and-swap on apply",
        ));
    }
    if let Some(digest) = resolved.runtime_digest.as_deref() {
        let key = ResourceKey::new("runtime", "settings");
        if ledger.resource_digest(&context.ownership_id(), &key) != Some(digest) {
            plan.warnings.push(
                "runtime settings are daemon-owned singletons; concurrent manual changes are guarded by expected_revision during apply".to_string(),
            );
        }
    }
    Ok(())
}

async fn add_connector_actions<C>(
    client: &C,
    _context: &StackContext,
    resolved: &ResolvedStack,
    _ledger: &ApplyLedger,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for connector in &resolved.connectors {
        let key = ResourceKey::new(
            "connector",
            &format!("{}/{}", connector.kind, connector.name),
        );
        let path = format!(
            "/v1/runtime/connectors/{}/{}",
            url_encode_path_segment(&connector.kind),
            url_encode_path_segment(&connector.name)
        );
        let live = client
            .get_json_optional::<crate::ConnectorView>(&path)
            .await?;
        let operation = if live.is_none() {
            "create"
        } else if live
            .as_ref()
            .is_some_and(|live| connector_matches(live, connector))
        {
            "noop"
        } else {
            "update"
        };
        plan.actions.push(StackAction::new(
            "connectors",
            key.kind,
            key.id,
            operation,
            "connector request payload is applied through the runtime connector API",
        ));
    }
    Ok(())
}

async fn add_persona_actions<C>(
    client: &C,
    resolved: &ResolvedStack,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for persona in &resolved.personas {
        let encoded = url_encode_path_segment(&persona.persona_id);
        let live = client
            .get_json_optional::<crate::PersonaView>(&format!("/v1/personas/{encoded}"))
            .await?;
        let operation = match live.as_ref() {
            None => "create",
            Some(live) if persona_matches(live, persona) => "noop",
            Some(_) => "update",
        };
        plan.actions.push(StackAction::new(
            "personas",
            "persona",
            persona.persona_id.clone(),
            operation,
            "persona state is compared through daemon readback",
        ));
    }
    Ok(())
}

async fn add_session_actions<C>(
    client: &C,
    resolved: &ResolvedStack,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for session in &resolved.sessions {
        let encoded = url_encode_path_segment(&session.session_id);
        let live = client
            .get_json_optional::<crate::SessionView>(&format!("/v1/sessions/{encoded}"))
            .await?;
        let operation = match live.as_ref() {
            None => "create",
            Some(live) if session_matches(live, session) => "noop",
            Some(live) if session.persona_id.is_none() && live.persona.as_ref().is_some() => {
                plan.errors.push(format!(
                    "session {} has a live persona but the desired stack omits persona_id; the current daemon API cannot clear a session persona",
                    session.session_id
                ));
                "blocked"
            }
            Some(_) => "update",
        };
        plan.actions.push(StackAction::new(
            "sessions",
            "session",
            session.session_id.clone(),
            operation,
            "session persona, scopes, route policy, operator config, and reply targets are reconciled through daemon APIs",
        ));
    }
    Ok(())
}

async fn add_schedule_actions<C>(
    client: &C,
    _context: &StackContext,
    resolved: &ResolvedStack,
    _ledger: &ApplyLedger,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    let live = client
        .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
        .await?;
    for schedule in &resolved.schedules {
        let found = live
            .iter()
            .find(|candidate| candidate.name == schedule.name);
        let key = ResourceKey::new("schedule", &schedule.name);
        let operation = match found {
            None => "create",
            Some(found) if schedule_view_matches(found, &schedule.request) => "noop",
            Some(_) => {
                plan.errors.push(format!(
                    "schedule {} exists with drift; schedules are immutable through the current API, use stack down on a ledger-owned schedule or create a new name",
                    schedule.name
                ));
                "blocked"
            }
        };
        plan.actions.push(StackAction::new(
            "schedules",
            key.kind,
            key.id,
            operation,
            "schedule identity is name-based in KheishStack and create-only in the daemon API",
        ));
    }
    Ok(())
}

async fn add_playbook_actions<C>(
    client: &C,
    resolved: &ResolvedStack,
    plan: &mut StackPlan,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for playbook in &resolved.playbooks {
        let validation = client
            .post_json::<_, crate::PlaybookValidationResult>(
                "/v1/playbooks/validate",
                &crate::ValidatePlaybookRequest {
                    manifest: playbook.manifest.clone(),
                },
            )
            .await?;
        if !validation.valid {
            plan.errors.extend(validation.errors);
        }
        plan.warnings.extend(validation.warnings);
        let digest = validation.digest.unwrap_or_else(|| playbook.digest.clone());
        let encoded = url_encode_path_segment(&playbook.manifest.playbook_id);
        let live = client
            .get_json_optional::<crate::PlaybookView>(&format!("/v1/playbooks/{encoded}"))
            .await?;
        let live_version = live
            .as_ref()
            .and_then(|view| {
                view.versions
                    .iter()
                    .find(|version| version.version == playbook.manifest.version)
            })
            .cloned();
        let has_version = live_version
            .as_ref()
            .is_some_and(|version| version.digest == digest);
        let operation = if has_version { "noop" } else { "create" };
        plan.actions.push(StackAction::new(
            "playbooks",
            "playbook",
            format!(
                "{}/{}",
                playbook.manifest.playbook_id, playbook.manifest.version
            ),
            operation,
            "playbook create is idempotent by manifest digest",
        ));
        if let Some(publish) = &playbook.publish {
            let release_matches = live_version.as_ref().is_some_and(|version| {
                version.status == publish.status && version.evidence_refs == publish.evidence_refs
            });
            plan.actions.push(StackAction::new(
                "playbooks",
                "playbook_release",
                format!(
                    "{}/{}",
                    playbook.manifest.playbook_id, playbook.manifest.version
                ),
                if publish.status == crate::PlaybookReleaseStatus::Draft || release_matches {
                    "noop"
                } else {
                    "update"
                },
                "release metadata is mutable and daemon-validated with expected digest",
            ));
        }
    }
    Ok(())
}

fn add_verification_actions(resolved: &ResolvedStack, plan: &mut StackPlan) {
    for probe in &resolved.verification {
        plan.actions.push(StackAction::new(
            "verification",
            "probe",
            probe.name.clone(),
            "verify",
            "existence probe runs after apply",
        ));
    }
}

fn add_ownership_diagnostics(ledger: &ApplyLedger, ownership_id: &str, plan: &mut StackPlan) {
    for action in &plan.actions {
        let Some(key) = resource_key_for_action(action) else {
            continue;
        };
        match ledger.owner_of_resource(&key) {
            Some(owner) if owner != ownership_id && requires_stack_resource_ownership(action) => plan.errors.push(format!(
                "resource {key} is already owned by stack `{owner}`; import or down that ownership before applying `{ownership_id}`"
            )),
            None if requires_existing_resource_ownership(action) => plan.errors.push(format!(
                "resource {key} already exists but is not owned by stack `{ownership_id}`; run stack import before applying"
            )),
            _ => {}
        }
    }
}

fn resource_key_for_action(action: &StackAction) -> Option<ResourceKey> {
    match action.resource_type.as_str() {
        "connector" | "persona" | "secret" | "session" | "schedule" | "playbook" => {
            Some(ResourceKey::new(&action.resource_type, &action.resource_id))
        }
        "runtime" => Some(ResourceKey::new("runtime", "settings")),
        _ => None,
    }
}

fn requires_existing_resource_ownership(action: &StackAction) -> bool {
    matches!(action.operation.as_str(), "noop" | "update" | "apply")
}

fn requires_stack_resource_ownership(action: &StackAction) -> bool {
    matches!(
        action.operation.as_str(),
        "create" | "noop" | "update" | "apply"
    )
}

fn managed_secret_modified_after_apply(
    ledger: &ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
    live: &kheish_auth::AuthSlotStatus,
) -> bool {
    ledger
        .resource_last_applied_at_ms(ownership_id, key)
        .is_none_or(|last_applied_at_ms| live.updated_at_ms > last_applied_at_ms)
}

fn enforce_desired_resource_ownership(
    ledger: &ApplyLedger,
    ownership_id: &str,
    resolved: &ResolvedStack,
) -> Result<()> {
    for key in resolved
        .desired_resource_keys()
        .into_iter()
        .filter_map(|key| ResourceKey::parse(&key).ok())
    {
        if let Some(owner) = ledger.owner_of_resource(&key)
            && owner != ownership_id
        {
            return Err(crate::problems::DaemonProblem::conflict(
                "stacks",
                "stack_ownership_conflict",
                format!("resource {key} is already owned by stack `{owner}`"),
            )
            .into());
        }
    }
    Ok(())
}

async fn apply_secrets<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    allow_secret_env: bool,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    let ownership_id = context.ownership_id();
    for secret in &resolved.secrets {
        let encoded = url_encode_path_segment(&secret.slot);
        let live = client
            .get_json_optional::<kheish_auth::AuthSlotStatus>(&format!(
                "/v1/runtime/secrets/{encoded}"
            ))
            .await?;
        if let Some(env_name) = secret.value_env.as_deref() {
            if !allow_secret_env {
                bail!(
                    "secret {} uses value_env={}; pass --allow-secret-env to import and fingerprint it",
                    secret.slot,
                    env_name
                );
            }
            let key = ResourceKey::new("secret", &secret.slot);
            if live.is_some() {
                ensure_existing_resource_owned(ledger, &ownership_id, &key)?;
            }
            let fingerprint = secret_fingerprint(&ledger.ledger_salt, &secret.slot, env_name)?;
            if live.is_some()
                && live
                    .as_ref()
                    .is_some_and(|status| status.provider == secret.provider)
                && ledger
                    .secret_fingerprint(&ownership_id, &secret.slot)
                    .is_some_and(|stored| stored == fingerprint)
                && live.as_ref().is_some_and(|status| {
                    !managed_secret_modified_after_apply(ledger, &ownership_id, &key, status)
                })
            {
                continue;
            }
            if live.is_none() {
                claim_resource_for_create(
                    ledger_path,
                    ledger,
                    &ownership_id,
                    &key,
                    fingerprint.clone(),
                )
                .await?;
            }
            let value = std::env::var(env_name)
                .with_context(|| format!("failed to read environment variable {env_name}"))?;
            let record = build_secret_record(secret, value)?;
            let write_result = if live.is_none() {
                client.create_secret_if_absent(&secret.slot, &record).await
            } else {
                client.post_json("/v1/runtime/secrets", &record).await
            };
            if let Err(error) = write_result {
                if live.is_none() {
                    let live_probe = client
                        .get_json_optional::<kheish_auth::AuthSlotStatus>(&format!(
                            "/v1/runtime/secrets/{encoded}"
                        ))
                        .await
                        .map(|status| status.is_some());
                    return rollback_or_preserve_failed_create_claim(
                        ledger_path,
                        ledger,
                        &ownership_id,
                        &key,
                        error,
                        live_probe,
                    )
                    .await;
                }
                return Err(error);
            }
            let resource_digest = fingerprint.clone();
            ledger.record_secret(&ownership_id, &secret.slot, fingerprint);
            ledger.record_resource(&ownership_id, &key, resource_digest);
            ledger.save(ledger_path).await?;
            report.applied.push(StackAction::new(
                "secrets",
                "secret",
                secret.slot.clone(),
                if live.is_some() { "update" } else { "create" },
                "secret was written through the daemon secret API; value is represented only by ledger fingerprint",
            ));
        } else {
            match live.as_ref() {
                None => bail!(
                    "required secret {} is missing and no value_env was provided",
                    secret.slot
                ),
                Some(status) if status.provider != secret.provider => bail!(
                    "required secret {} exists with provider {} but manifest requires {} and has no value_env source",
                    secret.slot,
                    status.provider,
                    secret.provider
                ),
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn ensure_existing_resource_owned(
    ledger: &ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
) -> Result<()> {
    match ledger.owner_of_resource(key) {
        Some(owner) if owner == ownership_id => Ok(()),
        Some(owner) => Err(crate::problems::DaemonProblem::conflict(
            "stacks",
            "stack_ownership_conflict",
            format!("resource {key} is already owned by stack `{owner}`"),
        )
        .into()),
        None => Err(crate::problems::DaemonProblem::conflict(
            "stacks",
            "stack_ownership_required",
            format!(
                "resource {key} already exists but is not owned by stack `{ownership_id}`; run stack import before applying"
            ),
        )
        .into()),
    }
}

async fn claim_resource_for_create(
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
    desired_digest: String,
) -> Result<()> {
    if ledger.claim_resource(ownership_id, key, desired_digest)? {
        ledger.save(ledger_path).await?;
    }
    Ok(())
}

async fn rollback_resource_claim(
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
) {
    if ledger.remove_resource_claim(ownership_id, key) {
        let _ = ledger.save(ledger_path).await;
    }
}

async fn rollback_or_preserve_failed_create_claim(
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
    error: anyhow::Error,
    live_probe: Result<bool>,
) -> Result<()> {
    let preserve_context = if create_error_confirms_existing_resource(&error) {
        None
    } else {
        match live_probe {
            Ok(true) => Some(format!(
                "{key} became visible after create failed; preserved pending stack ownership claim for retry recovery"
            )),
            Ok(false) => None,
            Err(probe_error) => Some(format!(
                "could not verify whether {key} was created after create failed: {probe_error}; preserved pending stack ownership claim for retry recovery"
            )),
        }
    };

    if let Some(context) = preserve_context {
        return Err(error.context(context));
    }

    rollback_resource_claim(ledger_path, ledger, ownership_id, key).await;
    Err(error)
}

fn create_error_confirms_existing_resource(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("already exists"))
}

async fn promote_pending_resource_if_present(
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    ownership_id: &str,
    key: &ResourceKey,
    desired_digest: String,
) -> Result<()> {
    if ledger.has_pending_resource(ownership_id, key) {
        ledger.record_resource(ownership_id, key, desired_digest);
        ledger.save(ledger_path).await?;
    }
    Ok(())
}

fn build_secret_record(
    secret: &ResolvedSecret,
    value: String,
) -> Result<kheish_auth::AuthSlotRecord> {
    let slot = kheish_auth::AuthSlotId::new(secret.slot.clone());
    let record = match secret.provider {
        kheish_auth::AuthProvider::Generic => {
            kheish_auth::GenericAuthBackend::static_secret_record(slot, &value)?
        }
        kheish_auth::AuthProvider::OpenAi => {
            kheish_auth::OpenAiAuthBackend::static_api_key_record(slot, &value, None, None)?
        }
        kheish_auth::AuthProvider::Anthropic => {
            kheish_auth::AnthropicAuthBackend::static_api_key_record(slot, &value)?
        }
        kheish_auth::AuthProvider::Google => {
            kheish_auth::GoogleAuthBackend::static_api_key_record(slot, &value)?
        }
        kheish_auth::AuthProvider::OpenRouter => {
            kheish_auth::OpenRouterAuthBackend::static_api_key_record(slot, &value)?
        }
        kheish_auth::AuthProvider::XAi => {
            kheish_auth::XAiAuthBackend::static_api_key_record(slot, &value)?
        }
        kheish_auth::AuthProvider::McpOAuth => {
            bail!(
                "value_env cannot create MCP OAuth records; import that account with `secrets import-codex` or an MCP OAuth flow first"
            )
        }
    };
    ensure_connector_secret_slot_record_allowed(&record)?;
    Ok(record)
}

async fn apply_runtime<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    let Some(digest) = resolved.runtime_digest.as_deref() else {
        return Ok(());
    };
    let mut runtime = client
        .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
        .await?;
    if let Some(permission_mode) = resolved.runtime.permission_mode.as_ref()
        && runtime.permission_mode != *permission_mode
    {
        runtime = client
            .post_json::<_, crate::RuntimeSettingsView>(
                "/v1/runtime/permission-mode",
                &crate::SetPermissionModeRequest {
                    mode: permission_mode.clone(),
                    expected_revision: Some(runtime.config.revision),
                },
            )
            .await?;
        report.applied.push(StackAction::new(
            "runtime",
            "runtime",
            "permission_mode",
            "update",
            "updated with expected_revision",
        ));
    }
    if let Some(debug_level) = resolved.runtime.debug_level
        && runtime.debug_level != debug_level
    {
        client
            .post_json::<_, crate::RuntimeSettingsView>(
                "/v1/runtime/debug-level",
                &crate::SetDebugLevelRequest {
                    level: debug_level,
                    expected_revision: Some(runtime.config.revision),
                },
            )
            .await?;
        report.applied.push(StackAction::new(
            "runtime",
            "runtime",
            "debug_level",
            "update",
            "updated with expected_revision",
        ));
    }
    ledger.record_resource(
        &context.ownership_id(),
        &ResourceKey::new("runtime", "settings"),
        digest.to_string(),
    );
    ledger.save(ledger_path).await?;
    Ok(())
}

async fn apply_connectors<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for connector in &resolved.connectors {
        let path = format!(
            "/v1/runtime/connectors/{}/{}",
            url_encode_path_segment(&connector.kind),
            url_encode_path_segment(&connector.name)
        );
        let live = client
            .get_json_optional::<crate::ConnectorView>(&path)
            .await?;
        let key = ResourceKey::new(
            "connector",
            &format!("{}/{}", connector.kind, connector.name),
        );
        if live.is_some() {
            ensure_existing_resource_owned(ledger, &context.ownership_id(), &key)?;
        }
        if live
            .as_ref()
            .is_some_and(|live| connector_matches(live, connector))
        {
            promote_pending_resource_if_present(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                connector.digest.clone(),
            )
            .await?;
            continue;
        }
        if live.is_none() {
            claim_resource_for_create(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                connector.digest.clone(),
            )
            .await?;
        }
        let write_result = if live.is_none() {
            client
                .put_connector_if_absent(&connector.kind, &connector.name, &connector.spec)
                .await
        } else {
            client.put_json(&path, &connector.spec).await
        };
        if let Err(error) = write_result {
            if live.is_none() {
                let live_probe = client
                    .get_json_optional::<crate::ConnectorView>(&path)
                    .await
                    .map(|view| view.is_some());
                return rollback_or_preserve_failed_create_claim(
                    ledger_path,
                    ledger,
                    &context.ownership_id(),
                    &key,
                    error,
                    live_probe,
                )
                .await;
            }
            return Err(error);
        }
        ledger.record_resource(&context.ownership_id(), &key, connector.digest.clone());
        ledger.save(ledger_path).await?;
        report.applied.push(StackAction::new(
            "connectors",
            key.kind,
            key.id,
            "apply",
            "connector payload written through runtime connector API",
        ));
    }
    Ok(())
}

async fn apply_personas<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for persona in &resolved.personas {
        let encoded = url_encode_path_segment(&persona.persona_id);
        let live = client
            .get_json_optional::<crate::PersonaView>(&format!("/v1/personas/{encoded}"))
            .await?;
        let key = ResourceKey::new("persona", &persona.persona_id);
        if live.is_some() {
            ensure_existing_resource_owned(ledger, &context.ownership_id(), &key)?;
        }
        if live
            .as_ref()
            .is_some_and(|live| persona_matches(live, persona))
        {
            promote_pending_resource_if_present(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                persona.digest.clone(),
            )
            .await?;
            continue;
        }
        if live.is_some() {
            client
                .put_json::<_, crate::PersonaView>(
                    &format!("/v1/personas/{encoded}"),
                    &crate::UpdatePersonaRequest {
                        display_name: Some(persona.display_name.clone()),
                        soul: Some(persona.soul.clone()),
                        metadata: Some(persona.metadata.clone()),
                        capability_scope: persona.capability_scope.clone(),
                        default_skills: Some(persona.default_skills.clone()),
                    },
                )
                .await?;
        } else {
            claim_resource_for_create(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                persona.digest.clone(),
            )
            .await?;
            let request = crate::CreatePersonaRequest {
                persona_id: Some(persona.persona_id.clone()),
                display_name: persona.display_name.clone(),
                soul: persona.soul.clone(),
                metadata: Some(persona.metadata.clone()),
                capability_scope: persona.capability_scope.clone(),
                default_skills: Some(persona.default_skills.clone()),
            };
            if let Err(error) = client.create_persona_if_absent(&request).await {
                let live_probe = client
                    .get_json_optional::<crate::PersonaView>(&format!("/v1/personas/{encoded}"))
                    .await
                    .map(|view| view.is_some());
                return rollback_or_preserve_failed_create_claim(
                    ledger_path,
                    ledger,
                    &context.ownership_id(),
                    &key,
                    error,
                    live_probe,
                )
                .await;
            }
        }
        ledger.record_resource(&context.ownership_id(), &key, persona.digest.clone());
        ledger.save(ledger_path).await?;
        report.applied.push(StackAction::new(
            "personas",
            "persona",
            persona.persona_id.clone(),
            if live.is_some() { "update" } else { "create" },
            "persona reconciled through daemon API",
        ));
    }
    Ok(())
}

async fn apply_sessions<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for session in &resolved.sessions {
        let encoded = url_encode_path_segment(&session.session_id);
        let live = client
            .get_json_optional::<crate::SessionView>(&format!("/v1/sessions/{encoded}"))
            .await?;
        let key = ResourceKey::new("session", &session.session_id);
        if live.is_some() {
            ensure_existing_resource_owned(ledger, &context.ownership_id(), &key)?;
        }
        if live
            .as_ref()
            .is_some_and(|live| session_matches(live, session))
        {
            promote_pending_resource_if_present(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                session.digest.clone(),
            )
            .await?;
            continue;
        }
        if live.is_none() {
            claim_resource_for_create(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                session.digest.clone(),
            )
            .await?;
            let request = crate::CreateSessionRequest {
                session_id: Some(session.session_id.clone()),
                thread_id: session.thread_id.clone(),
                persona_id: session.persona_id.clone(),
                capability_scope: session.capability_scope.clone(),
                credential_scope: session.credential_scope.clone(),
            };
            if let Err(error) = client.create_session_if_absent(&request).await {
                let live_probe = client
                    .get_json_optional::<crate::SessionView>(&format!("/v1/sessions/{encoded}"))
                    .await
                    .map(|view| view.is_some());
                return rollback_or_preserve_failed_create_claim(
                    ledger_path,
                    ledger,
                    &context.ownership_id(),
                    &key,
                    error,
                    live_probe,
                )
                .await;
            }
        } else {
            if let Some(persona_id) = session.persona_id.clone() {
                client
                    .post_json::<_, crate::SessionView>(
                        &format!("/v1/sessions/{encoded}/persona"),
                        &crate::SetSessionPersonaRequest { persona_id },
                    )
                    .await?;
            }
            client
                .post_json::<_, crate::SessionView>(
                    &format!("/v1/sessions/{encoded}/capability-scope"),
                    &crate::SetSessionCapabilityScopeRequest {
                        capability_scope: session.capability_scope.clone(),
                    },
                )
                .await?;
            client
                .post_json::<_, crate::SessionView>(
                    &format!("/v1/sessions/{encoded}/credential-scope"),
                    &crate::SetSessionCredentialScopeRequest {
                        credential_scope: session.credential_scope.clone(),
                    },
                )
                .await?;
        }
        client
            .post_json::<_, crate::SessionView>(
                &format!("/v1/sessions/{encoded}/route-policy"),
                &crate::SetSessionRoutePolicyRequest {
                    route_policy: session.route_policy.clone(),
                },
            )
            .await?;
        let desired_operator = session.operator.clone().unwrap_or_default();
        if desired_operator.enabled && desired_operator.allow_notify {
            apply_session_reply_targets(client, &encoded, session).await?;
            apply_session_operator(client, &encoded, desired_operator).await?;
        } else {
            apply_session_operator(client, &encoded, desired_operator).await?;
            apply_session_reply_targets(client, &encoded, session).await?;
        }
        ledger.record_resource(&context.ownership_id(), &key, session.digest.clone());
        ledger.save(ledger_path).await?;
        report.applied.push(StackAction::new(
            "sessions",
            "session",
            session.session_id.clone(),
            if live.is_some() { "update" } else { "create" },
            "session reconciled through daemon API",
        ));
    }
    Ok(())
}

async fn apply_session_operator<C>(
    client: &C,
    encoded_session_id: &str,
    operator: kheish_types::SessionOperatorConfig,
) -> Result<crate::SessionView>
where
    C: StackControlPlane + Sync,
{
    client
        .post_json::<_, crate::SessionView>(
            &format!("/v1/sessions/{encoded_session_id}/operator"),
            &crate::SetSessionOperatorConfigRequest { operator },
        )
        .await
}

async fn apply_session_reply_targets<C>(
    client: &C,
    encoded_session_id: &str,
    session: &ResolvedSession,
) -> Result<crate::SessionView>
where
    C: StackControlPlane + Sync,
{
    client
        .post_json::<_, crate::SessionView>(
            &format!("/v1/sessions/{encoded_session_id}/reply-targets"),
            &crate::SetSessionReplyTargetsRequest {
                reply_targets: session.reply_targets.clone().unwrap_or_default(),
            },
        )
        .await
}

async fn apply_schedules<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    let live = client
        .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
        .await?;
    for schedule in &resolved.schedules {
        if let Some(existing) = live
            .iter()
            .find(|candidate| candidate.name == schedule.name)
        {
            let key = ResourceKey::new("schedule", &schedule.name);
            ensure_existing_resource_owned(ledger, &context.ownership_id(), &key)?;
            if schedule_view_matches(existing, &schedule.request) {
                promote_pending_resource_if_present(
                    ledger_path,
                    ledger,
                    &context.ownership_id(),
                    &key,
                    schedule.digest.clone(),
                )
                .await?;
                continue;
            }
            bail!(
                "schedule {} exists with drift; schedules are immutable through the current API",
                schedule.name
            );
        }
        let key = ResourceKey::new("schedule", &schedule.name);
        claim_resource_for_create(
            ledger_path,
            ledger,
            &context.ownership_id(),
            &key,
            schedule.digest.clone(),
        )
        .await?;
        if let Err(error) = client.create_schedule_if_absent(&schedule.request).await {
            let live_probe = client
                .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
                .await
                .map(|schedules| {
                    schedules
                        .iter()
                        .any(|candidate| candidate.name == schedule.name)
                });
            return rollback_or_preserve_failed_create_claim(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                error,
                live_probe,
            )
            .await;
        }
        ledger.record_resource(&context.ownership_id(), &key, schedule.digest.clone());
        ledger.save(ledger_path).await?;
        report.applied.push(StackAction::new(
            "schedules",
            key.kind,
            key.id,
            "create",
            "schedule created through daemon API",
        ));
    }
    Ok(())
}

async fn apply_playbooks<C>(
    client: &C,
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger_path: &Path,
    ledger: &mut ApplyLedger,
    report: &mut StackApplyReport,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for playbook in &resolved.playbooks {
        let encoded_playbook_id = url_encode_path_segment(&playbook.manifest.playbook_id);
        let live = client
            .get_json_optional::<crate::PlaybookView>(&format!(
                "/v1/playbooks/{encoded_playbook_id}"
            ))
            .await?;
        let live_version = live.as_ref().and_then(|view| {
            view.versions
                .iter()
                .find(|version| version.version == playbook.manifest.version)
        });
        let key = ResourceKey::new(
            "playbook",
            &format!(
                "{}/{}",
                playbook.manifest.playbook_id, playbook.manifest.version
            ),
        );
        if live_version.is_some() {
            ensure_existing_resource_owned(ledger, &context.ownership_id(), &key)?;
        }
        let already_stored = live_version.is_some_and(|version| version.digest == playbook.digest);
        let digest = if already_stored {
            promote_pending_resource_if_present(
                ledger_path,
                ledger,
                &context.ownership_id(),
                &key,
                playbook.digest.clone(),
            )
            .await?;
            playbook.digest.clone()
        } else {
            if live_version.is_none() {
                claim_resource_for_create(
                    ledger_path,
                    ledger,
                    &context.ownership_id(),
                    &key,
                    playbook.digest.clone(),
                )
                .await?;
            }
            let request = crate::CreatePlaybookRequest {
                manifest: playbook.manifest.clone(),
            };
            let created = match client.create_playbook_version_if_absent(&request).await {
                Ok(created) => created,
                Err(error) => {
                    if live_version.is_none() {
                        let live_probe = client
                            .get_json_optional::<crate::PlaybookView>(&format!(
                                "/v1/playbooks/{encoded_playbook_id}"
                            ))
                            .await
                            .map(|view| {
                                view.is_some_and(|view| {
                                    view.versions
                                        .iter()
                                        .any(|version| version.version == playbook.manifest.version)
                                })
                            });
                        return rollback_or_preserve_failed_create_claim(
                            ledger_path,
                            ledger,
                            &context.ownership_id(),
                            &key,
                            error,
                            live_probe,
                        )
                        .await;
                    }
                    return Err(error);
                }
            };
            created
                .versions
                .iter()
                .find(|version| version.version == playbook.manifest.version)
                .map(|version| version.digest.clone())
                .unwrap_or_else(|| playbook.digest.clone())
        };
        if let Some(publish) = &playbook.publish
            && publish.status != crate::PlaybookReleaseStatus::Draft
        {
            let release_matches = live_version.is_some_and(|version| {
                version.status == publish.status && version.evidence_refs == publish.evidence_refs
            });
            if !release_matches {
                client
                    .post_json::<_, crate::PlaybookView>(
                        &format!("/v1/playbooks/{encoded_playbook_id}/publish"),
                        &crate::PublishPlaybookRequest {
                            version: playbook.manifest.version.clone(),
                            digest: digest.clone(),
                            status: Some(publish.status.clone()),
                            evidence_refs: publish.evidence_refs.clone(),
                        },
                    )
                    .await?;
            }
        }
        ledger.record_resource(&context.ownership_id(), &key, digest);
        ledger.save(ledger_path).await?;
        if !already_stored {
            report.applied.push(StackAction::new(
                "playbooks",
                key.kind,
                key.id,
                "apply",
                "playbook manifest stored idempotently by daemon digest",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn verify_stack<C>(
    client: &C,
    context: &StackContext,
) -> Result<StackVerificationReport>
where
    C: StackControlPlane + Sync,
{
    let (validation, resolved) = resolve_validated_stack(context).await?;
    let ledger_path = resolve_ledger_path(client, context.state_root_override.as_deref()).await?;
    let ledger = ApplyLedger::load_or_new(&ledger_path).await?;
    let mut checks = Vec::new();
    checks.extend(
        validation
            .errors
            .iter()
            .map(|error| StackVerificationCheck::failed("validate", error)),
    );
    checks.extend(run_mcp_requirement_checks(client, &resolved).await?);
    if resolved.runtime_digest.is_some() {
        let live = client
            .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
            .await?;
        checks.push(StackVerificationCheck::new(
            "runtime",
            "settings",
            runtime_matches(&live, &resolved.runtime),
            "runtime settings match desired fields",
        ));
    }
    let ownership_id = context.ownership_id();
    for secret in &resolved.secrets {
        let encoded = url_encode_path_segment(&secret.slot);
        let live = client
            .get_json_optional::<kheish_auth::AuthSlotStatus>(&format!(
                "/v1/runtime/secrets/{encoded}"
            ))
            .await?;
        let key = ResourceKey::new("secret", &secret.slot);
        let (ok, detail) = match live.as_ref() {
            None => (false, "required secret is missing".to_string()),
            Some(status) if status.provider != secret.provider => (
                false,
                format!(
                    "required secret provider mismatch: live provider {} does not match manifest provider {}",
                    status.provider, secret.provider
                ),
            ),
            Some(_)
                if secret.value_env.is_some()
                    && ledger.owner_of_resource(&key) != Some(ownership_id.as_str()) =>
            {
                (
                    false,
                    "managed secret is not owned by this stack ledger".to_string(),
                )
            }
            Some(status)
                if secret.value_env.is_some()
                    && managed_secret_modified_after_apply(
                        &ledger,
                        &ownership_id,
                        &key,
                        status,
                    ) =>
            {
                (
                    false,
                    "managed secret may have drifted after the last stack apply".to_string(),
                )
            }
            Some(_) => (
                true,
                "required secret exists and provider matches".to_string(),
            ),
        };
        checks.push(StackVerificationCheck::new(
            "secret",
            &secret.slot,
            ok,
            &detail,
        ));
    }
    for connector in &resolved.connectors {
        let path = format!(
            "/v1/runtime/connectors/{}/{}",
            url_encode_path_segment(&connector.kind),
            url_encode_path_segment(&connector.name)
        );
        let live = client
            .get_json_optional::<crate::ConnectorView>(&path)
            .await?;
        checks.push(StackVerificationCheck::new(
            "connector",
            &format!("{}/{}", connector.kind, connector.name),
            live.as_ref()
                .is_some_and(|live| connector_matches(live, connector)),
            "connector exists and matches desired fields",
        ));
    }
    for persona in &resolved.personas {
        let encoded = url_encode_path_segment(&persona.persona_id);
        let live = client
            .get_json_optional::<crate::PersonaView>(&format!("/v1/personas/{encoded}"))
            .await?;
        checks.push(StackVerificationCheck::new(
            "persona",
            &persona.persona_id,
            live.as_ref()
                .is_some_and(|live| persona_matches(live, persona)),
            "persona exists and matches desired fields",
        ));
    }
    for session in &resolved.sessions {
        let encoded = url_encode_path_segment(&session.session_id);
        let live = client
            .get_json_optional::<crate::SessionView>(&format!("/v1/sessions/{encoded}"))
            .await?;
        checks.push(StackVerificationCheck::new(
            "session",
            &session.session_id,
            live.as_ref()
                .is_some_and(|live| session_matches(live, session)),
            "session exists and matches desired fields",
        ));
    }
    let schedules = client
        .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
        .await?;
    for schedule in &resolved.schedules {
        checks.push(StackVerificationCheck::new(
            "schedule",
            &schedule.name,
            schedules.iter().any(|live| {
                live.name == schedule.name && schedule_view_matches(live, &schedule.request)
            }),
            "schedule exists and matches immutable fields",
        ));
    }
    for playbook in &resolved.playbooks {
        let encoded = url_encode_path_segment(&playbook.manifest.playbook_id);
        let live = client
            .get_json_optional::<crate::PlaybookView>(&format!("/v1/playbooks/{encoded}"))
            .await?;
        checks.push(StackVerificationCheck::new(
            "playbook",
            &format!(
                "{}/{}",
                playbook.manifest.playbook_id, playbook.manifest.version
            ),
            live.as_ref().is_some_and(|view| {
                view.versions.iter().any(|version| {
                    version.version == playbook.manifest.version
                        && version.digest == playbook.digest
                        && playbook.publish.as_ref().is_none_or(|publish| {
                            publish.status == crate::PlaybookReleaseStatus::Draft
                                || (version.status == publish.status
                                    && version.evidence_refs == publish.evidence_refs)
                        })
                })
            }),
            "playbook version, digest, and release metadata match",
        ));
    }
    for probe in &resolved.verification {
        checks.push(run_probe(client, probe).await?);
    }
    let valid = validation.valid && checks.iter().all(|check| check.ok);
    Ok(StackVerificationReport {
        stack: context.document.metadata.name.clone(),
        valid,
        checks,
        warnings: validation.warnings,
    })
}

async fn run_probe<C>(client: &C, probe: &ResolvedProbe) -> Result<StackVerificationCheck>
where
    C: StackControlPlane + Sync,
{
    match &probe.kind {
        ProbeKind::PersonaExists { persona_id } => {
            let encoded = url_encode_path_segment(persona_id);
            let live = client
                .get_json_optional::<crate::PersonaView>(&format!("/v1/personas/{encoded}"))
                .await?;
            Ok(StackVerificationCheck::new(
                "probe",
                &probe.name,
                live.is_some(),
                "persona exists",
            ))
        }
        ProbeKind::SessionExists { session_id } => {
            let encoded = url_encode_path_segment(session_id);
            let live = client
                .get_json_optional::<crate::SessionView>(&format!("/v1/sessions/{encoded}"))
                .await?;
            Ok(StackVerificationCheck::new(
                "probe",
                &probe.name,
                live.is_some(),
                "session exists",
            ))
        }
        ProbeKind::ScheduleExists { name } => {
            let schedules = client
                .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
                .await?;
            Ok(StackVerificationCheck::new(
                "probe",
                &probe.name,
                schedules.iter().any(|schedule| schedule.name == *name),
                "schedule exists by name",
            ))
        }
        ProbeKind::PlaybookVersionExists {
            playbook_id,
            version,
        } => {
            let encoded = url_encode_path_segment(playbook_id);
            let live = client
                .get_json_optional::<crate::PlaybookView>(&format!("/v1/playbooks/{encoded}"))
                .await?;
            Ok(StackVerificationCheck::new(
                "probe",
                &probe.name,
                live.as_ref().is_some_and(|view| {
                    view.versions
                        .iter()
                        .any(|candidate| candidate.version == *version)
                }),
                "playbook version exists",
            ))
        }
        ProbeKind::SecretExists { slot } => {
            let encoded = url_encode_path_segment(slot);
            let live = client
                .get_json_optional::<kheish_auth::AuthSlotStatus>(&format!(
                    "/v1/runtime/secrets/{encoded}"
                ))
                .await?;
            Ok(StackVerificationCheck::new(
                "probe",
                &probe.name,
                live.is_some(),
                "secret exists",
            ))
        }
    }
}

async fn run_mcp_requirement_checks<C>(
    client: &C,
    resolved: &ResolvedStack,
) -> Result<Vec<StackVerificationCheck>>
where
    C: StackControlPlane + Sync,
{
    if resolved.mcp_requirements.servers.is_empty() && resolved.mcp_requirements.tools.is_empty() {
        return Ok(Vec::new());
    }
    let runtime = client
        .get_json::<crate::RuntimeSettingsView>("/v1/runtime")
        .await?;
    let mut checks = Vec::new();
    checks.extend(resolved.mcp_requirements.servers.iter().map(|server| {
        let (ok, detail) = mcp_server_requirement_status(&runtime, server);
        StackVerificationCheck::new(
            "requirement",
            &format!("mcp_server/{}", server.name),
            ok,
            &detail,
        )
    }));
    checks.extend(resolved.mcp_requirements.tools.iter().map(|tool| {
        let active = mcp_tool_active(&runtime, tool);
        StackVerificationCheck::new(
            "requirement",
            &format!("mcp_tool/{tool}"),
            active,
            if active {
                "required MCP tool is active"
            } else {
                "required MCP tool is missing from the active MCP surface"
            },
        )
    }));
    Ok(checks)
}

fn runtime_matches(live: &crate::RuntimeSettingsView, desired: &StackRuntimeSpec) -> bool {
    desired
        .permission_mode
        .as_ref()
        .is_none_or(|mode| live.permission_mode == *mode)
        && desired
            .debug_level
            .is_none_or(|debug_level| live.debug_level == debug_level)
}

fn persona_matches(live: &crate::PersonaView, desired: &ResolvedPersona) -> bool {
    live.display_name == desired.display_name
        && live.soul == desired.soul
        && live.metadata == desired.metadata
        && live.capability_scope == desired.capability_scope.clone().unwrap_or_default()
        && live.default_skills == desired.default_skills
}

fn connector_matches(live: &crate::ConnectorView, desired: &ResolvedConnector) -> bool {
    match (desired.kind.as_str(), live) {
        ("http", crate::ConnectorView::Http(live)) => {
            let Ok(request) = parse_http_connector_request(&desired.name, &desired.spec) else {
                return false;
            };
            live.name == desired.name
                && live.actor_id == request.actor_id
                && live.fixed_session_id == request.fixed_session_id
                && connector_secret_matches(&live.bearer_token, request.bearer_token.as_ref())
                && connector_secret_matches(&live.hmac_secret, request.hmac_secret.as_ref())
                && live.allow_unauthenticated_ingress
                    == request.allow_unauthenticated_ingress.unwrap_or(false)
                && live.require_hmac_signature == request.require_hmac_signature.unwrap_or(false)
                && live.signature_max_age_secs == request.signature_max_age_secs.unwrap_or(300)
                && live.require_idempotency_key == request.require_idempotency_key.unwrap_or(true)
                && live.ingress_events_per_second == request.ingress_events_per_second.unwrap_or(60)
                && live.allow_payload_reply_targets
                    == request.allow_payload_reply_targets.unwrap_or(false)
                && live.default_reply_targets == request.default_reply_targets.unwrap_or_default()
                && live.default_binding_keys == request.default_binding_keys.unwrap_or_default()
                && live.session_policy == request.session_policy.unwrap_or_default()
        }
        _ => false,
    }
}

fn connector_secret_matches(
    live: &crate::ConnectorSecretView,
    desired: Option<&crate::ConnectorSecretInput>,
) -> bool {
    let Some(desired) = desired else {
        return !live.configured;
    };
    if let Some(secret_ref) = desired.secret_ref.as_deref() {
        return live.configured
            && live.source.as_deref() == Some("secret_ref")
            && live.secret_ref.as_deref() == Some(secret_ref);
    }
    if let Some(env) = desired.env.as_deref() {
        return live.configured
            && live.source.as_deref() == Some("env")
            && live.env.as_deref() == Some(env);
    }
    if desired.value.is_some() {
        return false;
    }
    !live.configured
}

fn session_matches(live: &crate::SessionView, desired: &ResolvedSession) -> bool {
    let live_persona_id = live
        .persona
        .as_ref()
        .map(|persona| persona.persona_id.as_str());
    let desired_reply_targets = desired
        .reply_targets
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(crate::SessionReplyTargetRequest::into_reply_handle)
        .collect::<Vec<_>>();
    live_persona_id == desired.persona_id.as_deref()
        && live.capability_scope == desired.capability_scope.clone().unwrap_or_default()
        && live.credential_scope == desired.credential_scope.clone().unwrap_or_default()
        && route_policy_matches(
            &live.route_policy,
            &desired.route_policy.clone().unwrap_or_default(),
        )
        && live.operator == desired.operator.clone().unwrap_or_default()
        && live.reply_targets == desired_reply_targets
}

/// Compares one desired route policy against the live one. The daemon
/// resolves route policies at write time — filling the generation config and
/// the route's default model — so exact equality would flag permanent drift
/// on every stack that pins a provider. Fields the manifest set explicitly
/// must match; fields it left unset accept the resolved value.
fn route_policy_matches(
    live: &kheish_types::SessionRoutePolicy,
    desired: &kheish_types::SessionRoutePolicy,
) -> bool {
    if desired.provider != live.provider {
        return false;
    }
    match (&desired.generation, &live.generation) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(desired_generation), Some(live_generation)) => {
            let mut desired_value = serde_json::to_value(desired_generation).unwrap_or_default();
            let live_value = serde_json::to_value(live_generation).unwrap_or_default();
            // Non-optional generation fields serialize their type defaults
            // even when the manifest never mentioned them; a default value in
            // the desired manifest means "unspecified", not a pin.
            let default_value =
                serde_json::to_value(kheish_runtime::ModelGenerationConfig::default())
                    .unwrap_or_default();
            if let (Value::Object(desired_map), Value::Object(default_map)) =
                (&mut desired_value, &default_value)
            {
                desired_map.retain(|key, value| default_map.get(key) != Some(value));
            }
            json_is_subset(&desired_value, &live_value)
        }
    }
}

/// Returns true when every field present in `desired` equals the matching
/// field in `live`, recursing through objects. Extra live fields are the
/// write-time resolution filling defaults, not drift.
fn json_is_subset(desired: &Value, live: &Value) -> bool {
    match (desired, live) {
        (Value::Object(desired_map), Value::Object(live_map)) => {
            desired_map.iter().all(|(key, desired_value)| {
                live_map
                    .get(key)
                    .is_some_and(|live_value| json_is_subset(desired_value, live_value))
            })
        }
        _ => desired == live,
    }
}

fn schedule_view_matches(
    live: &crate::ScheduleView,
    desired: &crate::ScheduleCreateRequest,
) -> bool {
    let base_matches = live.name == desired.name
        && live.target_session_id == desired.target_session_id
        && desired
            .target_agent_id
            .as_ref()
            .map(|target_agent_id| live.target_agent_id.as_ref() == Some(target_agent_id))
            .unwrap_or(true)
        && live.cadence == desired.cadence
        && live.max_executions == desired.max_executions
        && live.overlap_policy == desired.overlap_policy
        && live.misfire_policy == desired.misfire_policy
        && match live.definition_digest.as_deref() {
            Some(live_digest) => crate::scheduler::schedule_definition_digest(desired)
                .as_deref()
                .is_ok_and(|desired_digest| desired_digest == live_digest),
            None if desired.flow_start.is_some() => false,
            None => live.request == crate::summarize_schedule_create_request(desired),
        };
    base_matches
}

async fn fetch_live_resource_value<C>(client: &C, key: &ResourceKey) -> Result<Value>
where
    C: StackControlPlane + Sync,
{
    match key.kind.as_str() {
        "runtime" => client.get_json::<Value>("/v1/runtime").await,
        "persona" => {
            let encoded = url_encode_path_segment(&key.id);
            let value = client
                .get_json::<Value>(&format!("/v1/personas/{encoded}"))
                .await?;
            Ok(value)
        }
        "session" => {
            let encoded = url_encode_path_segment(&key.id);
            let value = client
                .get_json::<Value>(&format!("/v1/sessions/{encoded}"))
                .await?;
            Ok(value)
        }
        "schedule" => {
            let schedules = client.get_json::<Vec<Value>>("/v1/schedules").await?;
            schedules
                .into_iter()
                .find(|value| value.get("name").and_then(Value::as_str) == Some(key.id.as_str()))
                .ok_or_else(|| anyhow!("schedule {} not found", key.id))
        }
        "playbook" => {
            let (playbook_id, _) = key
                .id
                .split_once('/')
                .ok_or_else(|| anyhow!("playbook resource id must be playbook_id/version"))?;
            let encoded = url_encode_path_segment(playbook_id);
            client
                .get_json::<Value>(&format!("/v1/playbooks/{encoded}"))
                .await
        }
        "connector" => {
            let (kind, name) = key
                .id
                .split_once('/')
                .ok_or_else(|| anyhow!("connector resource id must be kind/name"))?;
            client
                .get_json::<Value>(&format!(
                    "/v1/runtime/connectors/{}/{}",
                    url_encode_path_segment(kind),
                    url_encode_path_segment(name)
                ))
                .await
        }
        "secret" => {
            let encoded = url_encode_path_segment(&key.id);
            client
                .get_json::<Value>(&format!("/v1/runtime/secrets/{encoded}"))
                .await
        }
        _ => bail!("unsupported import resource kind {}", key.kind),
    }
}

fn plan_prune(
    context: &StackContext,
    resolved: &ResolvedStack,
    ledger: &ApplyLedger,
) -> Vec<StackAction> {
    let desired = resolved
        .desired_resource_keys()
        .into_iter()
        .collect::<BTreeSet<_>>();
    ledger
        .stack(&context.ownership_id())
        .map(|stack| {
            stack
                .resources
                .keys()
                .filter_map(|key| ResourceKey::parse(key).ok())
                .filter(|key| !desired.contains(&key.to_string()))
                .map(|key| down_action_for_key(key, false))
                .collect()
        })
        .unwrap_or_default()
}

fn plan_down(context: &StackContext, ledger: &ApplyLedger) -> Vec<StackAction> {
    ledger
        .stack(&context.ownership_id())
        .map(|stack| {
            stack
                .resources
                .keys()
                .filter_map(|key| ResourceKey::parse(key).ok())
                .map(|key| down_action_for_key(key, true))
                .collect()
        })
        .unwrap_or_default()
}

fn down_action_for_key(key: ResourceKey, full_down: bool) -> StackAction {
    match key.kind.as_str() {
        "connector" => StackAction::new(
            "down",
            key.kind,
            key.id,
            "delete",
            "connectors have a DELETE endpoint",
        ),
        "schedule" => StackAction::new(
            "down",
            key.kind,
            key.id,
            "cancel",
            "schedules are canceled, not deleted",
        ),
        "session" => StackAction::new(
            "down",
            key.kind,
            key.id,
            "end",
            "sessions can be ended but not deleted",
        ),
        "secret" => StackAction::new(
            "down",
            key.kind,
            key.id,
            "blocked",
            if full_down {
                "secrets are daemon-global credentials; Stack v1alpha1 never deletes them automatically"
            } else {
                "secret omitted from desired stack but must be deleted explicitly after dependency review"
            },
        ),
        "persona" | "playbook" | "runtime" => StackAction::new(
            "down",
            key.kind,
            key.id,
            "blocked",
            if full_down {
                "daemon has no hard-delete endpoint for this resource; manual revoke/ignore semantics are required"
            } else {
                "resource omitted from desired stack but cannot be pruned automatically"
            },
        ),
        _ => StackAction::new(
            "down",
            key.kind,
            key.id,
            "blocked",
            "unknown ledger resource kind",
        ),
    }
}

async fn execute_down<C>(
    client: &C,
    context: &StackContext,
    actions: &[StackAction],
    ledger: &mut ApplyLedger,
    ledger_path: &Path,
) -> Result<()>
where
    C: StackControlPlane + Sync,
{
    for action in actions {
        if action.operation == "blocked" {
            continue;
        }
        let key = ResourceKey::new(&action.resource_type, &action.resource_id);
        let mut remove_claim = true;
        match action.resource_type.as_str() {
            "connector" => {
                let (kind, name) = action
                    .resource_id
                    .split_once('/')
                    .ok_or_else(|| anyhow!("connector resource id must be kind/name"))?;
                client
                    .delete_json::<serde_json::Value>(&format!(
                        "/v1/runtime/connectors/{}/{}",
                        url_encode_path_segment(kind),
                        url_encode_path_segment(name)
                    ))
                    .await?;
            }
            "schedule" => {
                let schedules = client
                    .get_json::<Vec<crate::ScheduleView>>("/v1/schedules")
                    .await?;
                if let Some(schedule) = schedules
                    .iter()
                    .find(|schedule| schedule.name == action.resource_id)
                {
                    if !schedule.status.is_terminal() {
                        client
                            .post_json::<_, crate::ScheduleMutationResponse>(
                                &format!(
                                    "/v1/schedules/{}/cancel",
                                    url_encode_path_segment(&schedule.schedule_id)
                                ),
                                &json!({}),
                            )
                            .await?;
                    }
                }
            }
            "session" => {
                let result = client
                    .post_json::<_, crate::SessionView>(
                        &format!(
                            "/v1/sessions/{}/end",
                            url_encode_path_segment(&action.resource_id)
                        ),
                        &crate::EndSessionRequest {
                            reason: Some("stack down".to_string()),
                        },
                    )
                    .await;
                if let Err(error) = result {
                    let message = error.to_string();
                    if message.contains("session_not_idle")
                        || message.contains("non-terminal work or live descendants")
                    {
                        remove_claim = false;
                    } else {
                        return Err(error);
                    }
                }
            }
            "secret" => {
                bail!(
                    "Stack down/prune refuses to delete secret `{}` automatically",
                    action.resource_id
                );
            }
            _ => continue,
        }
        if remove_claim {
            ledger.remove_resource(&context.ownership_id(), &key);
            ledger.save(ledger_path).await?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct StackContext {
    pub(crate) document: StackDocument,
    root: PathBuf,
    state_root_override: Option<PathBuf>,
    strict_scope_override: bool,
    allow_file_refs: bool,
}

impl StackContext {
    pub(crate) fn from_manifest(
        raw: &str,
        root: PathBuf,
        state_root_override: Option<PathBuf>,
        strict_scope_override: bool,
    ) -> Result<Self> {
        validate_stack_manifest_source(raw)?;
        let document =
            serde_yaml::from_str::<StackDocument>(raw).context("failed to parse KheishStack")?;
        Ok(Self {
            document,
            root,
            state_root_override,
            strict_scope_override,
            allow_file_refs: false,
        })
    }

    pub(crate) fn ownership_id(&self) -> String {
        self.document
            .spec
            .apply
            .ownership_id
            .clone()
            .unwrap_or_else(|| self.document.metadata.name.clone())
    }

    fn strict_scopes(&self) -> bool {
        let _deprecated_request_override = self.strict_scope_override;
        let _deprecated_manifest_switch = self.document.spec.apply.strict_scopes;
        true
    }

    fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        if !self.allow_file_refs {
            bail!(
                "file reference {} is not allowed through the daemon Stack API; submit a self-contained manifest",
                path.display()
            );
        }
        if path.is_absolute() {
            bail!(
                "absolute file reference {} is not allowed in KheishStack manifests",
                path.display()
            );
        }
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let resolved = root.join(path);
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", resolved.display()))?;
        if !canonical.starts_with(&root) {
            bail!(
                "file reference {} escapes KheishStack file_root {}",
                path.display(),
                root.display()
            );
        }
        Ok(canonical)
    }
}

pub fn validate_stack_manifest_source(raw: &str) -> Result<()> {
    if raw.len() > STACK_MANIFEST_BODY_LIMIT_BYTES {
        bail!(
            "KheishStack manifest exceeds the {} byte limit",
            STACK_MANIFEST_BODY_LIMIT_BYTES
        );
    }
    if let Some((line, column, token)) = find_yaml_anchor_or_alias(raw) {
        bail!(
            "KheishStack manifest uses YAML anchor/alias token `{token}` at line {line}, column {column}; anchors and aliases are disabled"
        );
    }
    Ok(())
}

fn find_yaml_anchor_or_alias(raw: &str) -> Option<(usize, usize, char)> {
    let mut block_scalar_indent: Option<usize> = None;
    for (line_index, line) in raw.lines().enumerate() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent >= block_indent {
                continue;
            }
            block_scalar_indent = None;
        }
        if let Some((column, token)) = find_yaml_anchor_or_alias_in_line(line) {
            return Some((line_index + 1, column, token));
        }
        if let Some(content_indent) = line_block_scalar_indent(line, indent) {
            block_scalar_indent = Some(content_indent);
        }
    }
    None
}

fn find_yaml_anchor_or_alias_in_line(line: &str) -> Option<(usize, char)> {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_single {
            if byte == b'\'' {
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                in_single = false;
            }
            index += 1;
            continue;
        }
        if in_double {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == b'"' {
                in_double = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'#' => break,
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'&' | b'*' if is_yaml_anchor_or_alias_candidate(bytes, index) => {
                return Some((index + 1, byte as char));
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn is_yaml_anchor_or_alias_candidate(bytes: &[u8], index: usize) -> bool {
    let Some(next) = bytes.get(index + 1) else {
        return false;
    };
    if !is_yaml_anchor_name_byte(*next) || !is_yaml_anchor_token_start(bytes, index) {
        return false;
    }
    let mut end = index + 2;
    while bytes
        .get(end)
        .is_some_and(|candidate| is_yaml_anchor_name_byte(*candidate))
    {
        end += 1;
    }
    bytes
        .get(end)
        .is_none_or(|candidate| is_yaml_anchor_token_boundary(*candidate))
}

fn is_yaml_anchor_token_start(bytes: &[u8], index: usize) -> bool {
    let Some(previous_index) = previous_non_space(bytes, index) else {
        return true;
    };
    if matches!(
        bytes[previous_index],
        b':' | b'-' | b'?' | b'[' | b'{' | b',' | b'('
    ) {
        return true;
    }
    yaml_tag_token_starts_node(bytes, previous_index)
}

fn yaml_tag_token_starts_node(bytes: &[u8], previous_index: usize) -> bool {
    let mut start = previous_index;
    while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if bytes.get(start) != Some(&b'!') {
        return false;
    }
    let Some(before_tag) = previous_non_space(bytes, start) else {
        return true;
    };
    matches!(
        bytes[before_tag],
        b':' | b'-' | b'?' | b'[' | b'{' | b',' | b'('
    )
}

fn previous_non_space(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        if !bytes[cursor].is_ascii_whitespace() {
            return Some(cursor);
        }
    }
    None
}

fn is_yaml_anchor_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_yaml_anchor_token_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b',' | b']' | b'}' | b':' | b'#' | b'\'' | b'"' | b'(' | b')'
        )
}

fn line_block_scalar_indent(line: &str, base_indent: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_single {
            if byte == b'\'' {
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                in_single = false;
            }
            index += 1;
            continue;
        }
        if in_double {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == b'"' {
                in_double = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'#' => break,
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'|' | b'>'
                if is_yaml_anchor_token_start(bytes, index)
                    && is_yaml_block_scalar_header_tail(bytes, index + 1).is_some() =>
            {
                let explicit_indent = is_yaml_block_scalar_header_tail(bytes, index + 1)
                    .expect("block scalar tail was just validated");
                return Some(yaml_block_scalar_content_indent(
                    bytes,
                    base_indent,
                    index,
                    explicit_indent,
                ));
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn is_yaml_block_scalar_header_tail(bytes: &[u8], mut index: usize) -> Option<Option<usize>> {
    let mut seen_chomp = false;
    let mut indent = None;
    while let Some(byte) = bytes.get(index).copied() {
        match byte {
            b'+' | b'-' if !seen_chomp => {
                seen_chomp = true;
                index += 1;
            }
            b'1'..=b'9' if indent.is_none() => {
                indent = Some((byte - b'0') as usize);
                index += 1;
            }
            b' ' | b'\t' => return Some(indent),
            b'#' => return Some(indent),
            _ => return None,
        }
    }
    Some(indent)
}

fn yaml_block_scalar_content_indent(
    bytes: &[u8],
    base_indent: usize,
    scalar_index: usize,
    explicit_indent: Option<usize>,
) -> usize {
    let mut node_indent = base_indent;
    if bytes.get(base_indent) == Some(&b'-')
        && bytes
            .get(base_indent + 1)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        && base_indent < scalar_index
    {
        let mut cursor = base_indent + 1;
        while cursor < scalar_index
            && bytes
                .get(cursor)
                .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        node_indent = cursor;
    }
    node_indent + explicit_indent.unwrap_or(1)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StackDocument {
    #[serde(rename = "apiVersion", alias = "api_version")]
    pub(crate) api_version: String,
    pub(crate) kind: String,
    pub(crate) metadata: StackMetadata,
    #[serde(default)]
    pub(crate) spec: StackSpec,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StackMetadata {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StackSpec {
    #[serde(default)]
    apply: StackApplyPolicy,
    #[serde(default)]
    requires: StackRequires,
    #[serde(default)]
    startup: StackStartupSpec,
    #[serde(default)]
    runtime: StackRuntimeSpec,
    #[serde(default)]
    connectors: Vec<StackConnectorSpec>,
    #[serde(default)]
    personas: Vec<StackPersonaSpec>,
    #[serde(default)]
    sessions: Vec<StackSessionSpec>,
    #[serde(default)]
    schedules: Vec<StackScheduleSpec>,
    #[serde(default)]
    playbooks: Vec<StackPlaybookSpec>,
    #[serde(default)]
    verification: Vec<StackProbeSpec>,
    #[serde(default)]
    agent_templates: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StackApplyPolicy {
    #[serde(default)]
    ownership_id: Option<String>,
    #[serde(default = "default_true")]
    strict_scopes: bool,
    #[serde(default)]
    allow_wildcard_scopes: bool,
    #[serde(default)]
    prune: bool,
    #[serde(default)]
    restart_policy: RestartPolicy,
}

impl Default for StackApplyPolicy {
    fn default() -> Self {
        Self {
            ownership_id: None,
            strict_scopes: true,
            allow_wildcard_scopes: false,
            prune: false,
            restart_policy: RestartPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestartPolicy {
    #[default]
    PlanOnly,
    Forbid,
    Allow,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackRequires {
    #[serde(default)]
    secrets: Vec<StackSecretRequirement>,
    #[serde(default)]
    mcp: StackMcpRequirements,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackSecretRequirement {
    #[serde(rename = "ref", alias = "slot")]
    slot: String,
    #[serde(default = "default_auth_provider")]
    provider: kheish_auth::AuthProvider,
    #[serde(default)]
    value_env: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackMcpRequirements {
    #[serde(default)]
    servers: Vec<StackMcpServerRequirementSpec>,
    #[serde(default)]
    tools: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum StackMcpServerRequirementSpec {
    Name(String),
    Detailed(StackMcpServerRequirementDetails),
}

impl StackMcpServerRequirementSpec {
    fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Detailed(details) => &details.name,
        }
    }

    fn resolved(&self) -> ResolvedMcpServerRequirement {
        match self {
            Self::Name(name) => ResolvedMcpServerRequirement {
                name: name.clone(),
                source: None,
                catalog_entry_id: None,
                uses_credentials: None,
                credential_secret_refs: Vec::new(),
            },
            Self::Detailed(details) => ResolvedMcpServerRequirement {
                name: details.name.clone(),
                source: details.source.clone(),
                catalog_entry_id: details.catalog_entry_id.clone(),
                uses_credentials: details.uses_credentials,
                credential_secret_refs: details.credential_secret_refs.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackMcpServerRequirementDetails {
    name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    catalog_entry_id: Option<String>,
    #[serde(default)]
    uses_credentials: Option<bool>,
    #[serde(default)]
    credential_secret_refs: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackStartupSpec {
    #[serde(default)]
    routes: Option<Value>,
    #[serde(default)]
    connectors_config: Option<Value>,
    #[serde(default)]
    mcp_profiles: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackRuntimeSpec {
    #[serde(default)]
    permission_mode: Option<kheish_runtime::PermissionMode>,
    #[serde(default)]
    debug_level: Option<kheish_runtime::DebugCaptureLevel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackConnectorSpec {
    kind: String,
    name: String,
    spec: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackPersonaSpec {
    persona_id: String,
    display_name: String,
    #[serde(default)]
    soul: Option<String>,
    #[serde(default)]
    soul_file: Option<PathBuf>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    capability_scope: Option<kheish_types::CapabilityScope>,
    #[serde(default)]
    default_skills: Vec<kheish_types::PersonaSkillAssignment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackSessionSpec {
    session_id: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    persona_id: Option<String>,
    #[serde(default)]
    capability_scope: Option<kheish_types::CapabilityScope>,
    #[serde(default)]
    credential_scope: Option<kheish_types::CredentialScope>,
    #[serde(default)]
    route_policy: Option<kheish_types::SessionRoutePolicy>,
    #[serde(default)]
    operator: Option<kheish_types::SessionOperatorConfig>,
    #[serde(default)]
    reply_targets: Option<Vec<crate::SessionReplyTargetRequest>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackScheduleSpec {
    name: String,
    target_session_id: String,
    #[serde(default)]
    target_agent_id: Option<String>,
    cadence: crate::ScheduleCadence,
    #[serde(default)]
    max_executions: Option<u64>,
    #[serde(default)]
    overlap_policy: crate::ScheduleOverlapPolicy,
    #[serde(default)]
    misfire_policy: crate::ScheduleMisfirePolicy,
    #[serde(default)]
    request: Option<StackRunRequestSpec>,
    #[serde(default)]
    observation_materialization: Option<crate::ObservationMaterializationRequest>,
    #[serde(default)]
    flow_start: Option<StackFlowStartSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackRunRequestSpec {
    #[serde(default, alias = "route_id")]
    provider: Option<String>,
    #[serde(default)]
    source_plugin: Option<String>,
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    actor_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    content_file: Option<PathBuf>,
    #[serde(default)]
    generation: Option<kheish_runtime::ModelGenerationConfig>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    binding_keys: Vec<String>,
    #[serde(default)]
    reply_plugin: Option<String>,
    #[serde(default)]
    reply_address: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackFlowStartSpec {
    #[serde(default)]
    flow_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    playbook_ref: StackPlaybookVersionRef,
    #[serde(default)]
    session_id: Option<String>,
    request: StackRunRequestSpec,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    evidence_refs: Vec<crate::FlowEvidenceRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackPlaybookVersionRef {
    playbook_id: String,
    version: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackPlaybookSpec {
    #[serde(default)]
    manifest: Option<crate::PlaybookManifest>,
    #[serde(default)]
    manifest_file: Option<PathBuf>,
    #[serde(default)]
    publish: Option<StackPlaybookPublishSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackPlaybookPublishSpec {
    status: crate::PlaybookReleaseStatus,
    #[serde(default)]
    evidence_refs: Vec<crate::FlowEvidenceRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum StackProbeSpec {
    PersonaExists {
        name: String,
        persona_id: String,
    },
    SessionExists {
        name: String,
        session_id: String,
    },
    ScheduleExists {
        name: String,
        schedule_name: String,
    },
    PlaybookVersionExists {
        name: String,
        playbook_id: String,
        version: String,
    },
    SecretExists {
        name: String,
        slot: String,
    },
}

impl StackProbeSpec {
    fn name(&self) -> &str {
        match self {
            Self::PersonaExists { name, .. }
            | Self::SessionExists { name, .. }
            | Self::ScheduleExists { name, .. }
            | Self::PlaybookVersionExists { name, .. }
            | Self::SecretExists { name, .. } => name,
        }
    }

    fn kind(&self) -> ProbeKind {
        match self {
            Self::PersonaExists { persona_id, .. } => ProbeKind::PersonaExists {
                persona_id: persona_id.clone(),
            },
            Self::SessionExists { session_id, .. } => ProbeKind::SessionExists {
                session_id: session_id.clone(),
            },
            Self::ScheduleExists { schedule_name, .. } => ProbeKind::ScheduleExists {
                name: schedule_name.clone(),
            },
            Self::PlaybookVersionExists {
                playbook_id,
                version,
                ..
            } => ProbeKind::PlaybookVersionExists {
                playbook_id: playbook_id.clone(),
                version: version.clone(),
            },
            Self::SecretExists { slot, .. } => ProbeKind::SecretExists { slot: slot.clone() },
        }
    }
}

#[derive(Clone, Debug)]
enum ProbeKind {
    PersonaExists {
        persona_id: String,
    },
    SessionExists {
        session_id: String,
    },
    ScheduleExists {
        name: String,
    },
    PlaybookVersionExists {
        playbook_id: String,
        version: String,
    },
    SecretExists {
        slot: String,
    },
}

fn default_true() -> bool {
    true
}

fn default_auth_provider() -> kheish_auth::AuthProvider {
    kheish_auth::AuthProvider::Generic
}

#[derive(Clone, Debug)]
struct ResolvedStack {
    startup_digest: Option<String>,
    runtime: StackRuntimeSpec,
    runtime_digest: Option<String>,
    secrets: Vec<ResolvedSecret>,
    mcp_requirements: ResolvedMcpRequirements,
    connectors: Vec<ResolvedConnector>,
    personas: Vec<ResolvedPersona>,
    sessions: Vec<ResolvedSession>,
    schedules: Vec<ResolvedSchedule>,
    playbooks: Vec<ResolvedPlaybook>,
    verification: Vec<ResolvedProbe>,
}

impl ResolvedStack {
    async fn from_context(context: &StackContext) -> Result<Self> {
        let startup_digest = if context.document.spec.startup.routes.is_some()
            || context.document.spec.startup.connectors_config.is_some()
            || !context.document.spec.startup.mcp_profiles.is_empty()
        {
            Some(digest_serializable(&context.document.spec.startup)?)
        } else {
            None
        };
        let runtime_digest = if context.document.spec.runtime.permission_mode.is_some()
            || context.document.spec.runtime.debug_level.is_some()
        {
            Some(digest_serializable(&context.document.spec.runtime)?)
        } else {
            None
        };
        let mut secrets = Vec::new();
        for secret in &context.document.spec.requires.secrets {
            secrets.push(ResolvedSecret {
                slot: secret.slot.clone(),
                provider: secret.provider,
                value_env: secret.value_env.clone(),
            });
        }
        let connectors = context
            .document
            .spec
            .connectors
            .iter()
            .map(|connector| {
                Ok(ResolvedConnector {
                    kind: connector.kind.clone(),
                    name: connector.name.clone(),
                    spec: connector.spec.clone(),
                    digest: digest_serializable(connector)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut personas = Vec::new();
        for persona in &context.document.spec.personas {
            let soul = match (&persona.soul, &persona.soul_file) {
                (Some(_), Some(_)) => {
                    bail!(
                        "persona {} must use either soul or soul_file, not both",
                        persona.persona_id
                    )
                }
                (Some(soul), None) => soul.clone(),
                (None, Some(path)) => {
                    let path = context.resolve_path(path)?;
                    read_stack_text_file(&path).await?
                }
                (None, None) => bail!("persona {} requires soul or soul_file", persona.persona_id),
            };
            let resolved = ResolvedPersona {
                persona_id: persona.persona_id.clone(),
                display_name: persona.display_name.clone(),
                soul,
                metadata: persona.metadata.clone().unwrap_or(Value::Null),
                capability_scope: persona
                    .capability_scope
                    .clone()
                    .map(|scope| scope.normalized()),
                default_skills: persona.default_skills.clone(),
                digest: String::new(),
            };
            let digest = digest_serializable(&resolved.desired_value())?;
            personas.push(ResolvedPersona { digest, ..resolved });
        }
        let sessions = context
            .document
            .spec
            .sessions
            .iter()
            .map(|session| {
                let resolved = ResolvedSession {
                    session_id: session.session_id.clone(),
                    thread_id: session.thread_id.clone(),
                    persona_id: session.persona_id.clone(),
                    capability_scope: session
                        .capability_scope
                        .clone()
                        .map(|scope| scope.normalized()),
                    credential_scope: session
                        .credential_scope
                        .clone()
                        .map(|scope| scope.normalized()),
                    route_policy: session.route_policy.clone(),
                    operator: session
                        .operator
                        .clone()
                        .map(crate::operator_contact::normalize_session_operator_config)
                        .transpose()?,
                    reply_targets: session.reply_targets.clone(),
                    digest: String::new(),
                };
                let digest = digest_serializable(&resolved.desired_value())?;
                Ok(ResolvedSession { digest, ..resolved })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut playbooks = Vec::new();
        for playbook in &context.document.spec.playbooks {
            let manifest = match (&playbook.manifest, &playbook.manifest_file) {
                (Some(_), Some(_)) => bail!("playbook must use either manifest or manifest_file"),
                (Some(manifest), None) => manifest.clone(),
                (None, Some(path)) => {
                    let path = context.resolve_path(path)?;
                    let raw = read_stack_text_file(&path).await?;
                    if path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                    {
                        serde_json::from_str(&raw)
                            .with_context(|| format!("failed to parse {}", path.display()))?
                    } else {
                        validate_stack_manifest_source(&raw)?;
                        serde_yaml::from_str(&raw)
                            .with_context(|| format!("failed to parse {}", path.display()))?
                    }
                }
                (None, None) => bail!("playbook requires manifest or manifest_file"),
            };
            let digest = digest_serializable(&manifest)?;
            playbooks.push(ResolvedPlaybook {
                manifest,
                publish: playbook.publish.clone(),
                digest,
            });
        }
        let mut schedules = Vec::new();
        for schedule in &context.document.spec.schedules {
            let payload_count = [
                schedule.request.is_some(),
                schedule.observation_materialization.is_some(),
                schedule.flow_start.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if payload_count != 1 {
                bail!("schedule {} must define exactly one payload", schedule.name);
            }
            let request = if let Some(request) = &schedule.request {
                Some(resolve_run_request(context, request).await?)
            } else {
                None
            };
            let flow_start = if let Some(flow_start) = &schedule.flow_start {
                Some(crate::StartFlowRequest {
                    flow_id: flow_start.flow_id.clone(),
                    idempotency_key: flow_start.idempotency_key.clone(),
                    playbook_ref: resolve_stack_playbook_ref(&flow_start.playbook_ref, &playbooks)?,
                    session_id: flow_start
                        .session_id
                        .clone()
                        .unwrap_or_else(|| schedule.target_session_id.clone()),
                    request: resolve_run_request(context, &flow_start.request).await?,
                    metadata: flow_start.metadata.clone().unwrap_or(Value::Null),
                    evidence_refs: flow_start.evidence_refs.clone(),
                })
            } else {
                None
            };
            let create = crate::ScheduleCreateRequest {
                name: schedule.name.clone(),
                target_session_id: schedule.target_session_id.clone(),
                target_agent_id: schedule.target_agent_id.clone(),
                owner_session_id: None,
                owner_agent_id: None,
                created_by_run_id: None,
                cadence: schedule.cadence.clone(),
                max_executions: schedule.max_executions,
                overlap_policy: schedule.overlap_policy.clone(),
                misfire_policy: schedule.misfire_policy.clone(),
                request,
                observation_materialization: schedule.observation_materialization.clone(),
                flow_start,
            };
            let digest = digest_serializable(&create)?;
            schedules.push(ResolvedSchedule {
                name: schedule.name.clone(),
                request: create,
                digest,
            });
        }
        let verification = context
            .document
            .spec
            .verification
            .iter()
            .map(|probe| ResolvedProbe {
                name: probe.name().to_string(),
                kind: probe.kind(),
            })
            .collect();
        Ok(Self {
            startup_digest,
            runtime: context.document.spec.runtime.clone(),
            runtime_digest,
            secrets,
            mcp_requirements: ResolvedMcpRequirements {
                servers: context
                    .document
                    .spec
                    .requires
                    .mcp
                    .servers
                    .iter()
                    .map(StackMcpServerRequirementSpec::resolved)
                    .collect(),
                tools: context.document.spec.requires.mcp.tools.clone(),
            },
            connectors,
            personas,
            sessions,
            schedules,
            playbooks,
            verification,
        })
    }

    fn desired_resource_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if self.runtime_digest.is_some() {
            keys.push(ResourceKey::new("runtime", "settings").to_string());
        }
        keys.extend(
            self.secrets
                .iter()
                .filter(|secret| secret.value_env.is_some())
                .map(|secret| ResourceKey::new("secret", &secret.slot).to_string()),
        );
        keys.extend(self.connectors.iter().map(|connector| {
            ResourceKey::new(
                "connector",
                &format!("{}/{}", connector.kind, connector.name),
            )
            .to_string()
        }));
        keys.extend(
            self.personas
                .iter()
                .map(|persona| ResourceKey::new("persona", &persona.persona_id).to_string()),
        );
        keys.extend(
            self.sessions
                .iter()
                .map(|session| ResourceKey::new("session", &session.session_id).to_string()),
        );
        keys.extend(
            self.schedules
                .iter()
                .map(|schedule| ResourceKey::new("schedule", &schedule.name).to_string()),
        );
        keys.extend(self.playbooks.iter().map(|playbook| {
            ResourceKey::new(
                "playbook",
                &format!(
                    "{}/{}",
                    playbook.manifest.playbook_id, playbook.manifest.version
                ),
            )
            .to_string()
        }));
        keys
    }
}

fn resolve_stack_playbook_ref(
    reference: &StackPlaybookVersionRef,
    playbooks: &[ResolvedPlaybook],
) -> Result<crate::PlaybookVersionRef> {
    if let Some(digest) = reference.digest.clone() {
        return Ok(crate::PlaybookVersionRef {
            playbook_id: reference.playbook_id.clone(),
            version: reference.version.clone(),
            digest,
        });
    }
    let Some(playbook) = playbooks.iter().find(|candidate| {
        candidate.manifest.playbook_id == reference.playbook_id
            && candidate.manifest.version == reference.version
    }) else {
        bail!(
            "flow_start.playbook_ref {}@{} omits digest but no matching spec.playbooks entry exists",
            reference.playbook_id,
            reference.version
        );
    };
    Ok(crate::PlaybookVersionRef {
        playbook_id: reference.playbook_id.clone(),
        version: reference.version.clone(),
        digest: playbook.digest.clone(),
    })
}

async fn resolve_run_request(
    context: &StackContext,
    request: &StackRunRequestSpec,
) -> Result<crate::SubmitInputRequest> {
    let content = match (&request.content, &request.content_file) {
        (Some(_), Some(_)) => bail!("schedule request must use either content or content_file"),
        (Some(content), None) => content.clone(),
        (None, Some(path)) => {
            let path = context.resolve_path(path)?;
            read_stack_text_file(&path).await?
        }
        (None, None) => String::new(),
    };
    Ok(crate::SubmitInputRequest {
        provider: request.provider.clone(),
        source_plugin: request.source_plugin.clone(),
        source_kind: request.source_kind.clone(),
        actor_id: request.actor_id.clone(),
        content,
        input_items: Vec::new(),
        attachments: Vec::new(),
        generation: request.generation.clone(),
        completion_requirements: None,
        metadata: Some(request.metadata.clone().unwrap_or(Value::Null)),
        binding_keys: request.binding_keys.clone(),
        reply_targets: Vec::new(),
        reply_plugin: request.reply_plugin.clone(),
        reply_address: request.reply_address.clone(),
    })
}

async fn read_stack_text_file(path: &Path) -> Result<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() > STACK_MANIFEST_BODY_LIMIT_BYTES as u64 {
        bail!(
            "{} exceeds the {} byte KheishStack file limit",
            path.display(),
            STACK_MANIFEST_BODY_LIMIT_BYTES
        );
    }
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    if raw.len() > STACK_MANIFEST_BODY_LIMIT_BYTES {
        bail!(
            "{} exceeds the {} byte KheishStack file limit",
            path.display(),
            STACK_MANIFEST_BODY_LIMIT_BYTES
        );
    }
    Ok(raw)
}

#[derive(Clone, Debug)]
struct ResolvedSecret {
    slot: String,
    provider: kheish_auth::AuthProvider,
    value_env: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ResolvedMcpRequirements {
    servers: Vec<ResolvedMcpServerRequirement>,
    tools: Vec<String>,
}

#[derive(Clone, Debug)]
struct ResolvedMcpServerRequirement {
    name: String,
    source: Option<String>,
    catalog_entry_id: Option<String>,
    uses_credentials: Option<bool>,
    credential_secret_refs: Vec<String>,
}

#[derive(Clone, Debug)]
struct ResolvedConnector {
    kind: String,
    name: String,
    spec: Value,
    digest: String,
}

#[derive(Clone, Debug)]
struct ResolvedPersona {
    persona_id: String,
    display_name: String,
    soul: String,
    metadata: Value,
    capability_scope: Option<kheish_types::CapabilityScope>,
    default_skills: Vec<kheish_types::PersonaSkillAssignment>,
    digest: String,
}

impl ResolvedPersona {
    fn desired_value(&self) -> Value {
        json!({
            "persona_id": self.persona_id,
            "display_name": self.display_name,
            "soul": self.soul,
            "metadata": self.metadata,
            "capability_scope": self.capability_scope,
            "default_skills": self.default_skills,
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedSession {
    session_id: String,
    thread_id: Option<String>,
    persona_id: Option<String>,
    capability_scope: Option<kheish_types::CapabilityScope>,
    credential_scope: Option<kheish_types::CredentialScope>,
    route_policy: Option<kheish_types::SessionRoutePolicy>,
    operator: Option<kheish_types::SessionOperatorConfig>,
    reply_targets: Option<Vec<crate::SessionReplyTargetRequest>>,
    digest: String,
}

impl ResolvedSession {
    fn desired_value(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "thread_id": self.thread_id,
            "persona_id": self.persona_id,
            "capability_scope": self.capability_scope,
            "credential_scope": self.credential_scope,
            "route_policy": self.route_policy,
            "operator": self.operator,
            "reply_targets": self.reply_targets,
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedSchedule {
    name: String,
    request: crate::ScheduleCreateRequest,
    digest: String,
}

#[derive(Clone, Debug)]
struct ResolvedPlaybook {
    manifest: crate::PlaybookManifest,
    publish: Option<StackPlaybookPublishSpec>,
    digest: String,
}

#[derive(Clone, Debug)]
struct ResolvedProbe {
    name: String,
    kind: ProbeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackValidation {
    pub stack: String,
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackPlan {
    pub stack: String,
    pub ownership_id: String,
    pub ledger_path: String,
    pub valid: bool,
    pub restart_required: bool,
    pub actions: Vec<StackAction>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub summary: StackPlanSummary,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StackPlanSummary {
    pub total: usize,
    pub create: usize,
    pub update: usize,
    pub noop: usize,
    pub blocked: usize,
    pub verify: usize,
}

impl StackPlanSummary {
    fn from_actions(actions: &[StackAction]) -> Self {
        let mut summary = StackPlanSummary {
            total: actions.len(),
            ..Self::default()
        };
        for action in actions {
            match action.operation.as_str() {
                "create" => summary.create += 1,
                "update" | "apply" => summary.update += 1,
                "noop" => summary.noop += 1,
                "blocked" => summary.blocked += 1,
                "verify" => summary.verify += 1,
                _ => {}
            }
        }
        summary
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackAction {
    pub phase: String,
    pub resource_type: String,
    pub resource_id: String,
    pub operation: String,
    pub reason: String,
}

impl StackAction {
    fn new(
        phase: impl Into<String>,
        resource_type: impl Into<String>,
        resource_id: impl Into<String>,
        operation: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            phase: phase.into(),
            resource_type: resource_type.into(),
            resource_id: resource_id.into(),
            operation: operation.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackApplyReport {
    pub stack: String,
    pub ownership_id: String,
    pub ledger_path: String,
    pub applied: Vec<StackAction>,
    pub verification: Option<StackVerificationReport>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<StackPlan>,
}

impl StackApplyReport {
    fn dry_run(plan: StackPlan) -> Self {
        Self {
            stack: plan.stack.clone(),
            ownership_id: plan.ownership_id.clone(),
            ledger_path: plan.ledger_path.clone(),
            applied: Vec::new(),
            verification: None,
            warnings: plan.warnings.clone(),
            plan: Some(plan),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackImportReport {
    pub stack: String,
    pub ownership_id: String,
    pub ledger_path: String,
    pub adopted: Vec<StackAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackDownReport {
    pub stack: String,
    pub ownership_id: String,
    pub ledger_path: String,
    pub executed: bool,
    pub actions: Vec<StackAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackVerificationReport {
    pub stack: String,
    pub valid: bool,
    pub checks: Vec<StackVerificationCheck>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackVerificationCheck {
    pub kind: String,
    pub target: String,
    pub ok: bool,
    pub detail: String,
}

impl StackVerificationCheck {
    fn new(kind: &str, target: &str, ok: bool, detail: &str) -> Self {
        Self {
            kind: kind.to_string(),
            target: target.to_string(),
            ok,
            detail: detail.to_string(),
        }
    }

    fn failed(kind: &str, detail: &str) -> Self {
        Self::new(kind, "stack", false, detail)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ResourceKey {
    kind: String,
    id: String,
}

impl ResourceKey {
    fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }

    fn parse(value: &str) -> Result<Self> {
        let (kind, id) = value
            .split_once('/')
            .ok_or_else(|| anyhow!("resource must be formatted as kind/id, got {value}"))?;
        if kind.trim().is_empty() || id.trim().is_empty() {
            bail!("resource must be formatted as kind/id, got {value}");
        }
        Ok(Self::new(kind, id))
    }
}

impl std::fmt::Display for ResourceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.kind, self.id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApplyLedger {
    version: u32,
    ledger_salt: String,
    #[serde(default)]
    stacks: BTreeMap<String, LedgerStack>,
}

impl ApplyLedger {
    async fn load_or_new(path: &Path) -> Result<Self> {
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let ledger = serde_json::from_slice::<ApplyLedger>(&bytes)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                if ledger.version != LEDGER_VERSION {
                    bail!(
                        "unsupported apply ledger version {} in {}",
                        ledger.version,
                        path.display()
                    );
                }
                Ok(ledger)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn new() -> Self {
        let mut salt = [0_u8; 32];
        rand::thread_rng().fill_bytes(&mut salt);
        Self {
            version: LEDGER_VERSION,
            ledger_salt: base64::engine::general_purpose::STANDARD_NO_PAD.encode(salt),
            stacks: BTreeMap::new(),
        }
    }

    async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = ledger_tmp_path(path);
        if let Err(error) = write_private_file(&tmp, &bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("failed to write {}", tmp.display()));
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("failed to replace {}", path.display()));
        }
        Ok(())
    }

    fn stack(&self, ownership_id: &str) -> Option<&LedgerStack> {
        self.stacks.get(ownership_id)
    }

    fn stack_mut(&mut self, ownership_id: &str) -> &mut LedgerStack {
        self.stacks.entry(ownership_id.to_string()).or_default()
    }

    fn owner_of_resource(&self, key: &ResourceKey) -> Option<&str> {
        let resource_key = key.to_string();
        self.stacks.iter().find_map(|(owner, stack)| {
            (stack.resources.contains_key(&resource_key)
                || stack.pending_resources.contains_key(&resource_key)
                || (key.kind == "secret" && stack.secrets.contains_key(&key.id)))
            .then_some(owner.as_str())
        })
    }

    fn claim_resource(
        &mut self,
        ownership_id: &str,
        key: &ResourceKey,
        desired_digest: String,
    ) -> Result<bool> {
        match self.owner_of_resource(key) {
            Some(owner) if owner == ownership_id => return Ok(false),
            Some(owner) => {
                return Err(crate::problems::DaemonProblem::conflict(
                    "stacks",
                    "stack_ownership_conflict",
                    format!("resource {key} is already owned by stack `{owner}`"),
                )
                .into());
            }
            None => {}
        }
        self.stack_mut(ownership_id).append_operation(
            "claim_resource",
            &key.to_string(),
            Some(desired_digest.clone()),
        );
        self.stack_mut(ownership_id).pending_resources.insert(
            key.to_string(),
            LedgerResource {
                desired_digest,
                last_applied_at_ms: crate::now_ms(),
            },
        );
        Ok(true)
    }

    fn record_resource(&mut self, ownership_id: &str, key: &ResourceKey, desired_digest: String) {
        self.stack_mut(ownership_id).append_operation(
            "record_resource",
            &key.to_string(),
            Some(desired_digest.clone()),
        );
        self.stack_mut(ownership_id)
            .pending_resources
            .remove(&key.to_string());
        self.stack_mut(ownership_id).resources.insert(
            key.to_string(),
            LedgerResource {
                desired_digest,
                last_applied_at_ms: crate::now_ms(),
            },
        );
    }

    fn remove_resource(&mut self, ownership_id: &str, key: &ResourceKey) {
        if let Some(stack) = self.stacks.get_mut(ownership_id) {
            stack.resources.remove(&key.to_string());
            stack.pending_resources.remove(&key.to_string());
            stack.append_operation("remove_resource", &key.to_string(), None);
        }
    }

    fn remove_resource_claim(&mut self, ownership_id: &str, key: &ResourceKey) -> bool {
        if let Some(stack) = self.stacks.get_mut(ownership_id)
            && stack.pending_resources.remove(&key.to_string()).is_some()
        {
            stack.append_operation("remove_resource_claim", &key.to_string(), None);
            return true;
        }
        false
    }

    fn has_pending_resource(&self, ownership_id: &str, key: &ResourceKey) -> bool {
        self.stacks
            .get(ownership_id)
            .is_some_and(|stack| stack.pending_resources.contains_key(&key.to_string()))
    }

    fn resource_digest(&self, ownership_id: &str, key: &ResourceKey) -> Option<&str> {
        self.stacks
            .get(ownership_id)?
            .resources
            .get(&key.to_string())
            .map(|resource| resource.desired_digest.as_str())
    }

    fn resource_last_applied_at_ms(&self, ownership_id: &str, key: &ResourceKey) -> Option<u64> {
        self.stacks
            .get(ownership_id)?
            .resources
            .get(&key.to_string())
            .map(|resource| resource.last_applied_at_ms)
    }

    fn record_secret(&mut self, ownership_id: &str, slot: &str, fingerprint: String) {
        self.stack_mut(ownership_id).append_operation(
            "record_secret",
            &ResourceKey::new("secret", slot).to_string(),
            Some(fingerprint.clone()),
        );
        self.stack_mut(ownership_id).secrets.insert(
            slot.to_string(),
            LedgerSecret {
                fingerprint,
                last_seen_at_ms: crate::now_ms(),
            },
        );
    }

    fn secret_fingerprint(&self, ownership_id: &str, slot: &str) -> Option<&str> {
        self.stacks
            .get(ownership_id)?
            .secrets
            .get(slot)
            .map(|secret| secret.fingerprint.as_str())
    }
}

#[derive(Debug)]
struct LedgerLock {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl LedgerLock {
    fn acquire(ledger_path: &Path) -> Result<Self> {
        let lock_path = ledger_path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        #[cfg(unix)]
        {
            use std::io::{Seek as _, Write as _};
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::OpenOptionsExt as _;

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&lock_path)
                .with_context(|| format!("failed to open {}", lock_path.display()))?;
            // SAFETY: flock only operates on this process-owned file descriptor.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(crate::problems::DaemonProblem::conflict(
                        "stacks",
                        "stack_ledger_locked",
                        format!(
                            "apply ledger is locked at {}; another stack operation may be running",
                            lock_path.display()
                        ),
                    )
                    .into());
                }
                return Err(error)
                    .with_context(|| format!("failed to lock {}", lock_path.display()));
            }

            let payload = format!(
                "pid={}\ncreated_at_ms={}\n",
                std::process::id(),
                crate::now_ms()
            );
            file.set_len(0)
                .with_context(|| format!("failed to truncate {}", lock_path.display()))?;
            file.seek(std::io::SeekFrom::Start(0))
                .with_context(|| format!("failed to seek {}", lock_path.display()))?;
            file.write_all(payload.as_bytes())
                .with_context(|| format!("failed to write {}", lock_path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", lock_path.display()))?;
            Ok(Self { file })
        }

        #[cfg(not(unix))]
        {
            use std::io::Write as _;

            let payload = format!(
                "pid={}\ncreated_at_ms={}\n",
                std::process::id(),
                crate::now_ms()
            );
            let mut file = match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(crate::problems::DaemonProblem::conflict(
                        "stacks",
                        "stack_ledger_locked",
                        format!(
                            "apply ledger is locked at {}; another stack operation may be running",
                            lock_path.display()
                        ),
                    )
                    .into());
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", lock_path.display()));
                }
            };
            file.write_all(payload.as_bytes())
                .with_context(|| format!("failed to write {}", lock_path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", lock_path.display()))?;
            Ok(Self { path: lock_path })
        }
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: flock only operates on this process-owned file descriptor.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LedgerStack {
    #[serde(default)]
    resources: BTreeMap<String, LedgerResource>,
    #[serde(default)]
    pending_resources: BTreeMap<String, LedgerResource>,
    #[serde(default)]
    secrets: BTreeMap<String, LedgerSecret>,
    #[serde(default)]
    operations: Vec<LedgerOperation>,
}

impl LedgerStack {
    fn append_operation(
        &mut self,
        action: impl Into<String>,
        resource_key: &str,
        desired_digest: Option<String>,
    ) {
        let index = self.operations.len() as u64;
        let at_ms = crate::now_ms();
        let previous_hash = self
            .operations
            .last()
            .map(|operation| operation.hash.clone());
        let action = action.into();
        let hash = ledger_operation_hash(
            index,
            at_ms,
            &action,
            resource_key,
            desired_digest.as_deref(),
            previous_hash.as_deref(),
        );
        self.operations.push(LedgerOperation {
            index,
            at_ms,
            action,
            resource_key: resource_key.to_string(),
            desired_digest,
            previous_hash,
            hash,
        });
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerResource {
    desired_digest: String,
    last_applied_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerSecret {
    fingerprint: String,
    last_seen_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerOperation {
    index: u64,
    at_ms: u64,
    action: String,
    resource_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desired_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_hash: Option<String>,
    hash: String,
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
}

fn ledger_tmp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(LEDGER_FILE);
    let nonce = rand::thread_rng().next_u64();
    path.with_file_name(format!(
        "{file_name}.tmp-{}-{nonce:016x}",
        std::process::id()
    ))
}

fn digest_serializable<T>(value: &T) -> Result<String>
where
    T: Serialize,
{
    digest_bytes(&serde_json::to_vec(value)?)
}

fn digest_json(value: &Value) -> Result<String> {
    digest_bytes(&serde_json::to_vec(value)?)
}

fn digest_bytes(bytes: &[u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn url_encode_path_segment(segment: &str) -> String {
    urlencoding::encode(segment).into_owned()
}

fn ensure_connector_secret_slot_record_allowed(record: &kheish_auth::AuthSlotRecord) -> Result<()> {
    if (record.slot_id.0.starts_with("connectors.") || record.slot_id.0.starts_with("mcp."))
        && !matches!(
            record.provider,
            kheish_auth::AuthProvider::Generic | kheish_auth::AuthProvider::McpOAuth
        )
    {
        bail!("connector and MCP secret slots must use generic opaque or MCP OAuth records");
    }
    if record.provider == kheish_auth::AuthProvider::McpOAuth
        && !record.slot_id.0.starts_with("mcp.oauth.")
    {
        bail!("MCP OAuth account slots must use the `mcp.oauth.` namespace");
    }
    Ok(())
}

fn ledger_operation_hash(
    index: u64,
    at_ms: u64,
    action: &str,
    resource_key: &str,
    desired_digest: Option<&str>,
    previous_hash: Option<&str>,
) -> String {
    let payload = json!({
        "index": index,
        "at_ms": at_ms,
        "action": action,
        "resource_key": resource_key,
        "desired_digest": desired_digest,
        "previous_hash": previous_hash,
    });
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_vec(&payload)
            .expect("ledger operation hash payload should serialize deterministically"),
    );
    hex::encode(hasher.finalize())
}

fn secret_fingerprint(salt: &str, slot: &str, env_name: &str) -> Result<String> {
    let value = std::env::var(env_name)
        .with_context(|| format!("failed to read environment variable {env_name}"))?;
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"\0");
    hasher.update(slot.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(raw: &str) -> StackContext {
        StackContext::from_manifest(raw, PathBuf::from("."), None, true).unwrap()
    }

    #[test]
    fn route_policy_verification_tolerates_write_time_resolution() {
        // The daemon fills the generation config and route default model when
        // a route policy is saved; a manifest pinning only the provider must
        // still verify clean.
        let desired: kheish_types::SessionRoutePolicy =
            serde_json::from_value(serde_json::json!({ "provider": "openai" })).unwrap();
        let live: kheish_types::SessionRoutePolicy = serde_json::from_value(serde_json::json!({
            "provider": "openai",
            "generation": { "model": "gpt-5.4", "tool_choice": { "type": "auto" } },
        }))
        .unwrap();
        assert!(route_policy_matches(&live, &desired));

        // An explicitly pinned model must still be compared.
        let desired_model: kheish_types::SessionRoutePolicy = serde_json::from_value(
            serde_json::json!({ "provider": "openai", "generation": { "model": "gpt-4o" } }),
        )
        .unwrap();
        assert!(!route_policy_matches(&live, &desired_model));

        // Type-default generation fields the manifest never mentioned must
        // not flag drift against a live session that tuned them.
        let live_tuned: kheish_types::SessionRoutePolicy =
            serde_json::from_value(serde_json::json!({
                "provider": "openai",
                "generation": { "model": "gpt-5.4", "allow_parallel_tool_calls": false },
            }))
            .unwrap();
        let desired_pinned_model: kheish_types::SessionRoutePolicy = serde_json::from_value(
            serde_json::json!({ "provider": "openai", "generation": { "model": "gpt-5.4" } }),
        )
        .unwrap();
        assert!(route_policy_matches(&live_tuned, &desired_pinned_model));

        // Provider drift stays drift.
        let desired_other: kheish_types::SessionRoutePolicy =
            serde_json::from_value(serde_json::json!({ "provider": "anthropic" })).unwrap();
        assert!(!route_policy_matches(&live, &desired_other));
    }

    fn errors_contain(validation: &StackValidation, needle: &str) -> bool {
        validation.errors.iter().any(|error| error.contains(needle))
    }

    fn warnings_contain(validation: &StackValidation, needle: &str) -> bool {
        validation
            .warnings
            .iter()
            .any(|warning| warning.contains(needle))
    }

    struct EmptyControlPlane;

    #[async_trait::async_trait]
    impl StackControlPlane for EmptyControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, _path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    struct RuntimeMcpControlPlane {
        runtime: crate::RuntimeSettingsView,
        playbook_validate_posts: Arc<std::sync::atomic::AtomicUsize>,
        state: Arc<RuntimeMcpControlPlaneState>,
    }

    #[derive(Default)]
    struct RuntimeMcpControlPlaneState {
        secrets: parking_lot::Mutex<BTreeMap<String, kheish_auth::AuthSlotStatus>>,
        personas: parking_lot::Mutex<BTreeMap<String, crate::PersonaView>>,
        sessions: parking_lot::Mutex<BTreeMap<String, crate::SessionView>>,
        playbooks: parking_lot::Mutex<BTreeMap<String, crate::PlaybookView>>,
        schedules: parking_lot::Mutex<Vec<crate::ScheduleView>>,
    }

    impl RuntimeMcpControlPlane {
        fn with_mcp(servers: &[&str], tools: &[&str]) -> Self {
            Self {
                runtime: crate::RuntimeSettingsView {
                    mcp: kheish_mcp::McpRuntimeSnapshot {
                        servers: servers
                            .iter()
                            .map(|server| kheish_mcp::McpServerSnapshot {
                                server: (*server).to_string(),
                                connected: true,
                                ..Default::default()
                            })
                            .collect(),
                        tool_names: tools.iter().map(|tool| (*tool).to_string()).collect(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                playbook_validate_posts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                state: Arc::default(),
            }
        }

        fn with_mcp_snapshot(servers: Vec<kheish_mcp::McpServerSnapshot>, tools: &[&str]) -> Self {
            Self {
                runtime: crate::RuntimeSettingsView {
                    mcp: kheish_mcp::McpRuntimeSnapshot {
                        servers,
                        tool_names: tools.iter().map(|tool| (*tool).to_string()).collect(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                playbook_validate_posts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                state: Arc::default(),
            }
        }

        fn playbook_validate_posts(&self) -> usize {
            self.playbook_validate_posts
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn auth_status(record: kheish_auth::AuthSlotRecord) -> kheish_auth::AuthSlotStatus {
            kheish_auth::AuthSlotStatus {
                slot_id: record.slot_id,
                provider: record.provider,
                mode: record.mode,
                summary: "configured".to_string(),
                updated_at_ms: record.updated_at_ms,
                details: BTreeMap::new(),
            }
        }

        fn create_persona(request: crate::CreatePersonaRequest) -> Result<crate::PersonaView> {
            let now_ms = crate::now_ms();
            Ok(crate::PersonaView {
                persona_id: request
                    .persona_id
                    .ok_or_else(|| anyhow!("persona create requires persona_id"))?,
                display_name: request.display_name,
                soul: request.soul,
                version: 1,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                capability_scope: request.capability_scope.unwrap_or_default(),
                default_skills: request.default_skills.unwrap_or_default(),
                metadata: request.metadata.unwrap_or(Value::Null),
            })
        }

        fn create_session(
            &self,
            request: crate::CreateSessionRequest,
        ) -> Result<crate::SessionView> {
            let session_id = request
                .session_id
                .ok_or_else(|| anyhow!("session create requires session_id"))?;
            let agent_id = format!("agent-{session_id}");
            let persona = request
                .persona_id
                .as_deref()
                .map(|persona_id| self.session_persona_summary(persona_id))
                .transpose()?;
            Ok(crate::SessionView {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                snapshot: Self::session_snapshot(&agent_id, &session_id),
                route_policy: Default::default(),
                goal: None,
                capability_scope: request.capability_scope.clone().unwrap_or_default(),
                effective_capability_scope: request.capability_scope.unwrap_or_default(),
                credential_scope: request.credential_scope.clone().unwrap_or_default(),
                effective_credential_scope: request.credential_scope.unwrap_or_default(),
                persona,
                operator: Default::default(),
                reply_targets: Vec::new(),
                outputs: Vec::new(),
            })
        }

        fn session_snapshot(
            agent_id: &str,
            session_id: &str,
        ) -> kheish_agent::ManagedAgentSnapshot {
            kheish_agent::ManagedAgentSnapshot {
                agent: kheish_agent::AgentRecord {
                    id: kheish_agent::AgentId(agent_id.to_string()),
                    parent: None,
                    name: Some(agent_id.replace('-', "_")),
                    path: Some(agent_id.replace('-', "_")),
                    nickname: None,
                    conversation: kheish_types::ConversationKey {
                        session_id: session_id.to_string(),
                        thread_id: None,
                    },
                    status: kheish_agent::AgentStatus::Idle,
                    retention: kheish_agent::ChildRetentionPolicy::Retain,
                    spawned_by_run_id: None,
                    spawn_request_id: None,
                    spawned_at_ms: crate::now_ms(),
                    settled_at_ms: None,
                    closed_at_ms: None,
                    subtasks: Vec::new(),
                    sidechain_session_id: None,
                    fork_context: None,
                    daemon_owned_worktree: None,
                },
                pending_approvals: Vec::new(),
                pending_questions: Vec::new(),
                last_assistant_message: None,
                journal_len: 0,
                checkpoint_len: 0,
                last_error: None,
            }
        }

        fn session_persona_summary(
            &self,
            persona_id: &str,
        ) -> Result<crate::SessionPersonaSummaryView> {
            let personas = self.state.personas.lock();
            let persona = personas
                .get(persona_id)
                .ok_or_else(|| anyhow!("persona {persona_id} not found"))?;
            Ok(crate::SessionPersonaSummaryView {
                persona_id: persona.persona_id.clone(),
                persona_version: persona.version,
                display_name: persona.display_name.clone(),
                bound_at_ms: crate::now_ms(),
            })
        }

        fn store_playbook(
            &self,
            request: crate::CreatePlaybookRequest,
        ) -> Result<crate::PlaybookView> {
            let digest = crate::playbooks::playbook_manifest_digest(&request.manifest)?;
            let now_ms = crate::now_ms();
            let mut playbooks = self.state.playbooks.lock();
            let entry = playbooks
                .entry(request.manifest.playbook_id.clone())
                .or_insert_with(|| crate::PlaybookView {
                    playbook_id: request.manifest.playbook_id.clone(),
                    latest_version: None,
                    active_version: None,
                    versions: Vec::new(),
                    selected_version: None,
                });
            if !entry
                .versions
                .iter()
                .any(|version| version.version == request.manifest.version)
            {
                entry.latest_version = Some(request.manifest.version.clone());
                entry.selected_version = Some(crate::PlaybookVersionRecord {
                    playbook_id: request.manifest.playbook_id.clone(),
                    version: request.manifest.version.clone(),
                    digest: digest.clone(),
                    manifest: request.manifest.clone(),
                    created_at_ms: now_ms,
                });
                entry.versions.push(crate::PlaybookVersionSummary {
                    version: request.manifest.version,
                    digest,
                    status: crate::PlaybookReleaseStatus::Draft,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                    evidence_refs: Vec::new(),
                    revoked_reason: None,
                });
            }
            Ok(entry.clone())
        }

        fn publish_playbook(
            &self,
            playbook_id: &str,
            request: crate::PublishPlaybookRequest,
        ) -> Result<crate::PlaybookView> {
            let now_ms = crate::now_ms();
            let mut playbooks = self.state.playbooks.lock();
            let view = playbooks
                .get_mut(playbook_id)
                .ok_or_else(|| anyhow!("playbook {playbook_id} not found"))?;
            let version = view
                .versions
                .iter_mut()
                .find(|version| version.version == request.version)
                .ok_or_else(|| anyhow!("playbook version {} not found", request.version))?;
            if version.digest != request.digest {
                bail!("playbook digest mismatch");
            }
            version.status = request
                .status
                .unwrap_or(crate::PlaybookReleaseStatus::Active);
            version.updated_at_ms = now_ms;
            version.evidence_refs = request.evidence_refs;
            version.revoked_reason = None;
            if version.status == crate::PlaybookReleaseStatus::Active {
                view.active_version = Some(version.version.clone());
            }
            Ok(view.clone())
        }
    }

    #[async_trait::async_trait]
    impl StackControlPlane for RuntimeMcpControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            match path {
                "/v1/runtime" => encode_response(self.runtime.clone()),
                "/v1/schedules" => encode_response(self.state.schedules.lock().clone()),
                _ => bail!("unexpected GET {path}"),
            }
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            let decoded_segments = decoded_path_segments(path)?;
            let segments = decoded_segments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            match segments.as_slice() {
                ["v1", "runtime", "secrets", slot] => self
                    .state
                    .secrets
                    .lock()
                    .get(*slot)
                    .cloned()
                    .map(encode_response)
                    .transpose(),
                ["v1", "personas", persona_id] => self
                    .state
                    .personas
                    .lock()
                    .get(*persona_id)
                    .cloned()
                    .map(encode_response)
                    .transpose(),
                ["v1", "sessions", session_id] => self
                    .state
                    .sessions
                    .lock()
                    .get(*session_id)
                    .cloned()
                    .map(encode_response)
                    .transpose(),
                ["v1", "playbooks", playbook_id] => self
                    .state
                    .playbooks
                    .lock()
                    .get(*playbook_id)
                    .cloned()
                    .map(encode_response)
                    .transpose(),
                _ => self.get_json(path).await.map(Some),
            }
        }

        async fn post_json<B, T>(&self, path: &str, body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            if path == "/v1/playbooks/validate" {
                self.playbook_validate_posts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let request = serde_json::from_value::<crate::ValidatePlaybookRequest>(
                    serde_json::to_value(body)?,
                )?;
                return encode_response(crate::playbooks::validate_playbook_manifest(
                    &request.manifest,
                ));
            }
            if path == "/v1/runtime/secrets" {
                let record = serde_json::from_value::<kheish_auth::AuthSlotRecord>(
                    serde_json::to_value(body)?,
                )?;
                let status = Self::auth_status(record);
                self.state
                    .secrets
                    .lock()
                    .insert(status.slot_id.0.clone(), status.clone());
                return encode_response(status);
            }
            if path == "/v1/personas" {
                let request = serde_json::from_value::<crate::CreatePersonaRequest>(
                    serde_json::to_value(body)?,
                )?;
                let persona = Self::create_persona(request)?;
                self.state
                    .personas
                    .lock()
                    .insert(persona.persona_id.clone(), persona.clone());
                return encode_response(persona);
            }
            if path == "/v1/sessions" {
                let request = serde_json::from_value::<crate::CreateSessionRequest>(
                    serde_json::to_value(body)?,
                )?;
                let session = self.create_session(request)?;
                self.state
                    .sessions
                    .lock()
                    .insert(session.session_id.clone(), session.clone());
                return encode_response(session);
            }
            if path == "/v1/schedules" {
                let request = serde_json::from_value::<crate::ScheduleCreateRequest>(
                    serde_json::to_value(body)?,
                )?;
                let mut schedules = self.state.schedules.lock();
                let schedule = crate::scheduler::build_schedule_record(
                    format!("schedule-{}", schedules.len() + 1),
                    crate::now_ms(),
                    request,
                )?
                .view;
                schedules.push(schedule.clone());
                return encode_response(schedule);
            }
            if path == "/v1/playbooks" {
                let request = serde_json::from_value::<crate::CreatePlaybookRequest>(
                    serde_json::to_value(body)?,
                )?;
                return encode_response(self.store_playbook(request)?);
            }
            let decoded_segments = decoded_path_segments(path)?;
            let segments = decoded_segments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            match segments.as_slice() {
                ["v1", "sessions", session_id, "persona"] => {
                    let request = serde_json::from_value::<crate::SetSessionPersonaRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let persona = self.session_persona_summary(&request.persona_id)?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    session.persona = Some(persona);
                    encode_response(session.clone())
                }
                ["v1", "sessions", session_id, "capability-scope"] => {
                    let request = serde_json::from_value::<crate::SetSessionCapabilityScopeRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    let scope = request.capability_scope.unwrap_or_default();
                    session.capability_scope = scope.clone();
                    session.effective_capability_scope = scope;
                    encode_response(session.clone())
                }
                ["v1", "sessions", session_id, "credential-scope"] => {
                    let request = serde_json::from_value::<crate::SetSessionCredentialScopeRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    let scope = request.credential_scope.unwrap_or_default();
                    session.credential_scope = scope.clone();
                    session.effective_credential_scope = scope;
                    encode_response(session.clone())
                }
                ["v1", "sessions", session_id, "route-policy"] => {
                    let request = serde_json::from_value::<crate::SetSessionRoutePolicyRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    session.route_policy = request.route_policy.unwrap_or_default();
                    encode_response(session.clone())
                }
                ["v1", "sessions", session_id, "operator"] => {
                    let request = serde_json::from_value::<crate::SetSessionOperatorConfigRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    session.operator = request.operator;
                    encode_response(session.clone())
                }
                ["v1", "sessions", session_id, "reply-targets"] => {
                    let request = serde_json::from_value::<crate::SetSessionReplyTargetsRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    let mut sessions = self.state.sessions.lock();
                    let session = sessions
                        .get_mut(*session_id)
                        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
                    session.reply_targets = request
                        .reply_targets
                        .into_iter()
                        .map(crate::SessionReplyTargetRequest::into_reply_handle)
                        .collect();
                    encode_response(session.clone())
                }
                ["v1", "playbooks", playbook_id, "publish"] => {
                    let request = serde_json::from_value::<crate::PublishPlaybookRequest>(
                        serde_json::to_value(body)?,
                    )?;
                    encode_response(self.publish_playbook(playbook_id, request)?)
                }
                _ => bail!("unexpected POST {path}"),
            }
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    #[derive(Default)]
    struct RecordingControlPlane {
        deletes: parking_lot::Mutex<Vec<String>>,
        posts: parking_lot::Mutex<Vec<String>>,
        schedules: parking_lot::Mutex<Vec<crate::ScheduleView>>,
    }

    #[async_trait::async_trait]
    impl StackControlPlane for RecordingControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(self.schedules.lock().clone());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, _path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            self.posts.lock().push(path.to_string());
            if path.starts_with("/v1/schedules/") && path.ends_with("/cancel") {
                let schedule = self
                    .schedules
                    .lock()
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow!("missing test schedule"))?;
                return encode_response(crate::ScheduleMutationResponse { schedule });
            }
            if path.starts_with("/v1/sessions/") && path.ends_with("/end") {
                bail!(
                    "409 Conflict (session_not_idle): session has non-terminal work or live descendants"
                );
            }
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            self.deletes.lock().push(path.to_string());
            encode_response(json!({ "accepted": true }))
        }
    }

    struct LiveSecretControlPlane {
        posts: parking_lot::Mutex<Vec<String>>,
        provider: kheish_auth::AuthProvider,
        updated_at_ms: u64,
        allow_secret_posts: bool,
    }

    impl Default for LiveSecretControlPlane {
        fn default() -> Self {
            Self {
                posts: parking_lot::Mutex::new(Vec::new()),
                provider: kheish_auth::AuthProvider::Generic,
                updated_at_ms: 1,
                allow_secret_posts: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl StackControlPlane for LiveSecretControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            if path == "/v1/runtime" {
                return encode_response(crate::RuntimeSettingsView::default());
            }
            if path.starts_with("/v1/runtime/secrets/") {
                return encode_response(self.secret_status());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            if path.starts_with("/v1/runtime/secrets/") {
                return encode_response(Some(self.secret_status()));
            }
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            self.posts.lock().push(path.to_string());
            if self.allow_secret_posts && path == "/v1/runtime/secrets" {
                return encode_response(self.secret_status());
            }
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    impl LiveSecretControlPlane {
        fn secret_status(&self) -> kheish_auth::AuthSlotStatus {
            kheish_auth::AuthSlotStatus {
                slot_id: kheish_auth::AuthSlotId::new("stack.test.LIVE_SECRET"),
                provider: self.provider,
                mode: kheish_auth::AuthMode::OpaqueSecret,
                summary: "configured".to_string(),
                updated_at_ms: self.updated_at_ms,
                details: BTreeMap::new(),
            }
        }
    }

    #[derive(Default)]
    struct RacePersonaControlPlane {
        persona_gets: parking_lot::Mutex<usize>,
        writes: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl StackControlPlane for RacePersonaControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/personas/race-persona" {
                let mut gets = self.persona_gets.lock();
                *gets += 1;
                if *gets == 1 {
                    return Ok(None);
                }
                let live = crate::PersonaView {
                    persona_id: "race-persona".to_string(),
                    display_name: "External Persona".to_string(),
                    soul: "Created outside stack.".to_string(),
                    version: 1,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                    capability_scope: kheish_types::CapabilityScope::default(),
                    default_skills: Vec::new(),
                    metadata: Value::Null,
                };
                return encode_response(Some(live));
            }
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            self.writes.lock().push(path.to_string());
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            self.writes.lock().push(path.to_string());
            bail!("unexpected PUT {path}")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    struct ClaimObservedConnectorControlPlane {
        ledger_path: PathBuf,
        observed_pending_claim: parking_lot::Mutex<bool>,
        created: parking_lot::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl StackControlPlane for ClaimObservedConnectorControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/runtime/connectors/http/claimed" {
                if *self.created.lock() {
                    return encode_response(Some(test_http_connector_view()));
                }
                return Ok(None);
            }
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            if path != "/v1/runtime/connectors/http/claimed" {
                bail!("unexpected PUT {path}");
            }
            let ledger = ApplyLedger::load_or_new(&self.ledger_path).await?;
            let key = ResourceKey::new("connector", "http/claimed");
            let stack = ledger
                .stack("connector-claim")
                .ok_or_else(|| anyhow!("missing connector-claim stack ledger"))?;
            if !stack.pending_resources.contains_key(&key.to_string()) {
                bail!("connector write happened before pending ledger claim was persisted");
            }
            *self.observed_pending_claim.lock() = true;
            *self.created.lock() = true;
            encode_response(test_http_connector_view())
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    struct RaceConnectorCreateControlPlane {
        ledger_path: PathBuf,
        observed_pending_claim: parking_lot::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl StackControlPlane for RaceConnectorCreateControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/runtime/connectors/http/race" {
                return Ok(None);
            }
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn put_connector_if_absent(
            &self,
            kind: &str,
            name: &str,
            _spec: &Value,
        ) -> Result<crate::ConnectorView> {
            if kind != "http" || name != "race" {
                bail!("unexpected connector create {kind}/{name}");
            }
            let ledger = ApplyLedger::load_or_new(&self.ledger_path).await?;
            let key = ResourceKey::new("connector", "http/race");
            let stack = ledger
                .stack("connector-race")
                .ok_or_else(|| anyhow!("missing connector-race stack ledger"))?;
            if !stack.pending_resources.contains_key(&key.to_string()) {
                bail!("connector create-if-absent happened before pending claim was persisted");
            }
            *self.observed_pending_claim.lock() = true;
            bail!("http connector race already exists")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    struct CommittedThenErroredConnectorControlPlane {
        created: parking_lot::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl StackControlPlane for CommittedThenErroredConnectorControlPlane {
        async fn get_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/schedules" {
                return encode_response(Vec::<crate::ScheduleView>::new());
            }
            bail!("unexpected GET {path}")
        }

        async fn get_json_optional<T>(&self, path: &str) -> Result<Option<T>>
        where
            T: DeserializeOwned + Send,
        {
            if path == "/v1/runtime/connectors/http/committed" {
                if *self.created.lock() {
                    return encode_response(Some(test_http_connector_view_named("committed")));
                }
                return Ok(None);
            }
            Ok(None)
        }

        async fn post_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected POST {path}")
        }

        async fn put_json<B, T>(&self, path: &str, _body: &B) -> Result<T>
        where
            B: Serialize + Sync + ?Sized,
            T: DeserializeOwned + Send,
        {
            bail!("unexpected PUT {path}")
        }

        async fn put_connector_if_absent(
            &self,
            kind: &str,
            name: &str,
            _spec: &Value,
        ) -> Result<crate::ConnectorView> {
            if kind != "http" || name != "committed" {
                bail!("unexpected connector create {kind}/{name}");
            }
            *self.created.lock() = true;
            bail!("connector registry reload failed after durable create")
        }

        async fn delete_json<T>(&self, path: &str) -> Result<T>
        where
            T: DeserializeOwned + Send,
        {
            bail!("unexpected DELETE {path}")
        }
    }

    fn test_http_connector_view() -> crate::ConnectorView {
        test_http_connector_view_named("claimed")
    }

    fn test_http_connector_view_named(name: &str) -> crate::ConnectorView {
        crate::ConnectorView::Http(crate::HttpConnectorView {
            source: crate::ConnectorSourceView::Daemon,
            name: name.to_string(),
            fixed_session_id: None,
            actor_id: None,
            bearer_token: crate::ConnectorSecretView::default(),
            hmac_secret: crate::ConnectorSecretView::default(),
            allow_unauthenticated_ingress: true,
            require_hmac_signature: false,
            signature_max_age_secs: 300,
            require_idempotency_key: true,
            ingress_events_per_second: 60,
            allow_payload_reply_targets: false,
            default_reply_targets: Vec::new(),
            default_binding_keys: Vec::new(),
            session_policy: serde_json::from_value(json!({
                "create_if_missing": true,
                "capability_scope": {
                    "skill_deny": ["*"],
                    "mcp_server_deny": ["*"],
                    "mcp_tool_deny": ["*"]
                },
                "credential_scope": {
                    "route_deny": ["*"],
                    "connector_deny": ["*"],
                    "connector_credential_deny": ["*"],
                    "mcp_server_deny": ["*"]
                }
            }))
            .expect("test connector session policy should deserialize"),
        })
    }

    fn empty_test_plan(context: &StackContext) -> StackPlan {
        StackPlan {
            stack: context.document.metadata.name.clone(),
            ownership_id: context.ownership_id(),
            ledger_path: String::new(),
            valid: true,
            restart_required: false,
            actions: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            summary: StackPlanSummary::default(),
        }
    }

    #[tokio::test]
    async fn validate_rejects_fail_open_connector_session_policy_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-scope-hole
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec.session_policy.capability_scope.skill"
        ));
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec.session_policy.credential_scope.route"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_fail_open_scopes_even_when_manifest_disables_strict_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: strict-manifest-noop
spec:
  apply:
    strict_scopes: false
  personas:
    - persona_id: operator
      display_name: Operator
      soul: Must still fail closed.
      capability_scope:
        mcp_server_allow: ["linear"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.personas[operator].capability_scope.skill"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_fail_open_scopes_even_when_request_disables_strict_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: strict-request-noop
spec:
  personas:
    - persona_id: operator
      display_name: Operator
      soul: Must still fail closed.
      capability_scope:
        mcp_server_allow: ["linear"]
"#;
        let context = StackContext::from_manifest(raw, PathBuf::from("."), None, false).unwrap();

        let validation = validate_stack_context(&context).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.personas[operator].capability_scope.skill"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_unknown_connector_spec_fields() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-unknown-field
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        require_hmac_signatre: true
        fixed_session_id: ingress-session
  sessions:
    - session_id: ingress-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec.require_hmac_signatre is not supported"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_inline_connector_secret_values() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-inline-secret
spec:
  sessions:
    - session_id: ingress-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  connectors:
    - kind: http
      name: ingress
      spec:
        fixed_session_id: ingress-session
        bearer_token:
          value: super-secret
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec.bearer_token.value is not supported in KheishStack"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_partial_deny_as_fail_closed_scope() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: partial-deny
spec:
  personas:
    - persona_id: operator
      display_name: Operator
      soul: Be constrained.
      capability_scope:
        skill_deny: ["bash"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.personas[operator].capability_scope.skill_deny must contain `*`"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_connector_without_fixed_session_or_session_policy() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-no-target
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec must set fixed_session_id or a fail-closed session_policy"
        ));
    }

    #[tokio::test]
    async fn validate_accepts_fail_closed_connector_session_policy_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-scoped
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(validation.valid, "{:?}", validation.errors);
    }

    #[tokio::test]
    async fn validate_accepts_fixed_session_id_declared_in_stack() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-fixed-session
spec:
  sessions:
    - session_id: ingress-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        fixed_session_id: ingress-session
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(validation.valid, "{:?}", validation.errors);
    }

    #[tokio::test]
    async fn validate_rejects_external_session_persona_under_strict_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: external-session-persona
spec:
  sessions:
    - session_id: triage
      persona_id: external-persona
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.sessions[triage].persona_id `external-persona` must reference a persona declared in this stack"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_external_connector_policy_persona_under_strict_scopes() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: external-connector-persona
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          persona_id: external-persona
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.connectors[ingress].spec.session_policy.persona_id `external-persona` must reference a persona declared in this stack"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_schedule_targeting_external_session_without_provider() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: external-schedule-target
spec:
  schedules:
    - name: external-session-schedule
      target_session_id: external-session
      cadence:
        type: interval
        every_seconds: 60
      request:
        content: run on external session
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[external-session-schedule].target_session_id `external-session` must reference a session declared in this stack"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_observation_schedule_target_mismatch() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: observation-schedule-target
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  schedules:
    - name: observation-schedule
      target_session_id: declared-session
      cadence:
        type: interval
        every_seconds: 60
      observation_materialization:
        target_session_id: external-session
        selection:
          type: observation_ids
          observation_ids: ["obs-1"]
        request:
          content: summarize observation
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[observation-schedule].observation_materialization.target_session_id must match target_session_id `declared-session`"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_observation_schedule_provider_outside_session_scope() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: observation-provider-scope
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  schedules:
    - name: observation-schedule
      target_session_id: declared-session
      cadence:
        type: interval
        every_seconds: 60
      observation_materialization:
        target_session_id: declared-session
        selection:
          type: observation_ids
          observation_ids: ["obs-1"]
        request:
          provider: openai
          content: summarize observation
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[observation-schedule].observation_materialization.request.provider `openai` is not allowed"
        ));
    }

    #[tokio::test]
    async fn resolved_stack_accepts_scheduled_flow_start_with_default_session_and_content_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("prompt.md"), "Run the feature Flow.")
            .expect("write prompt");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-start
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  playbooks:
    - manifest:
        playbook_id: feature-flow
        version: "1"
        title: Feature Flow
        objective: Run a feature workflow.
        phases:
          - phase_id: run
            objective: Run the scheduled feature workflow.
        acceptance_criteria:
          - The scheduled Flow creates a root run.
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
        request:
          content_file: prompt.md
"#;
        let mut context =
            StackContext::from_manifest(raw, temp.path().to_path_buf(), None, true).unwrap();
        context.allow_file_refs = true;
        let validation = validate_stack_context(&context).await.unwrap();
        assert!(validation.valid, "{:?}", validation.errors);

        let resolved = ResolvedStack::from_context(&context).await.unwrap();
        let flow_start = resolved.schedules[0]
            .request
            .flow_start
            .as_ref()
            .expect("flow_start");
        assert_eq!(flow_start.session_id, "declared-session");
        assert_eq!(flow_start.request.content, "Run the feature Flow.");
        assert!(!flow_start.playbook_ref.digest.is_empty());
    }

    #[tokio::test]
    async fn schedule_view_matches_detects_flow_start_definition_drift() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-drift
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  playbooks:
    - manifest:
        playbook_id: feature-flow
        version: "1"
        title: Feature Flow
        objective: Run a generic feature workflow.
        phases:
          - phase_id: run
            objective: Run the scheduled feature workflow.
        acceptance_criteria:
          - The scheduled Flow creates a root run.
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
        request:
          content: run
"#;
        let resolved = ResolvedStack::from_context(&context(raw)).await.unwrap();
        let desired = resolved.schedules[0].request.clone();
        let record =
            crate::scheduler::build_schedule_record("schedule-1".to_string(), 1, desired.clone())
                .unwrap();
        assert!(schedule_view_matches(&record.view, &desired));

        let mut drifted = desired.clone();
        drifted
            .flow_start
            .as_mut()
            .expect("flow_start")
            .playbook_ref
            .digest = "different-digest".to_string();

        assert!(!schedule_view_matches(&record.view, &drifted));
    }

    #[tokio::test]
    async fn validate_rejects_flow_start_provider_outside_session_scope() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-provider-scope
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
          digest: digest-1
        request:
          provider: openai
          content: run
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[scheduled-flow].flow_start.request.provider `openai` is not allowed"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_flow_start_playbook_default_provider_outside_session_scope() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-runtime-default-scope
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["openai"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  playbooks:
    - manifest:
        playbook_id: feature-flow
        version: "1"
        title: Feature Flow
        objective: Run a generic feature workflow.
        runtime_defaults:
          provider: openai
        phases:
          - phase_id: run
            objective: Run the scheduled feature workflow.
        acceptance_criteria:
          - The scheduled Flow creates a root run.
      publish:
        status: active
        evidence_refs:
          - kind: test
            id: release
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
        request:
          content: run
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[scheduled-flow].flow_start.request.provider `openai` is not allowed"
        ));
    }

    #[tokio::test]
    async fn validate_warns_when_scheduled_flow_references_draft_same_stack_playbook() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-draft-warning
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  playbooks:
    - manifest:
        playbook_id: feature-flow
        version: "1"
        title: Feature Flow
        objective: Run a generic feature workflow.
        phases:
          - phase_id: run
            objective: Run the scheduled feature workflow.
        acceptance_criteria:
          - The scheduled Flow creates a root run.
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
        request:
          content: run
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(validation.valid, "{:?}", validation.errors);
        assert!(warnings_contain(
            &validation,
            "flow_start references same-stack playbook feature-flow@1 with non-startable release status"
        ));
    }

    #[tokio::test]
    async fn sessions_accept_generic_operator_contact_policy() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: operator-contact
spec:
  sessions:
    - session_id: feature-loop
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
      reply_targets:
        - type: http
          url: https://example.com/kheish/operator
      operator:
        enabled: true
        display_name: Project operator
        communication_style: concise and human
        allow_notify: true
        allow_questions: true
"#;
        let resolved = ResolvedStack::from_context(&context(raw)).await.unwrap();
        let session = resolved.sessions.first().expect("session");
        let operator = session.operator.as_ref().expect("operator policy");
        assert!(operator.enabled);
        assert_eq!(operator.display_name.as_deref(), Some("Project operator"));
        assert_eq!(
            session.desired_value()["operator"]["communication_style"],
            json!("concise and human")
        );
        assert_eq!(session.reply_targets.as_ref().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn validate_accepts_operator_questions_without_reply_targets() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: operator-questions
spec:
  sessions:
    - session_id: feature-loop
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
      operator:
        enabled: true
        allow_notify: false
        allow_questions: true
"#;
        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(validation.valid, "{:?}", validation.errors);
    }

    #[tokio::test]
    async fn sessions_canonicalize_inactive_operator_contact_policy() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: inactive-operator
spec:
  sessions:
    - session_id: feature-loop
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
      operator:
        enabled: false
        display_name: Project operator
        communication_style: concise
        allow_notify: false
        allow_questions: false
"#;
        let resolved = ResolvedStack::from_context(&context(raw)).await.unwrap();
        let session = resolved.sessions.first().expect("session");

        assert_eq!(
            session.operator.as_ref(),
            Some(&kheish_types::SessionOperatorConfig::default())
        );
    }

    #[tokio::test]
    async fn validate_rejects_operator_notifications_without_reply_targets() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: operator-notify
spec:
  sessions:
    - session_id: feature-loop
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
      operator:
        enabled: true
        allow_notify: true
        allow_questions: true
"#;
        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.sessions[feature-loop].operator.allow_notify requires at least one reply_targets entry"
        ));
    }

    #[tokio::test]
    async fn validate_rejects_flow_start_scalar_request_metadata() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow-metadata
spec:
  sessions:
    - session_id: declared-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  schedules:
    - name: scheduled-flow
      target_session_id: declared-session
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
          digest: digest-1
        request:
          content: run
          metadata: scalar
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.schedules[scheduled-flow].flow_start.request.metadata must be an object"
        ));
    }

    #[tokio::test]
    async fn import_rejects_invalid_strict_scopes_before_ledger_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: import-external-persona
spec:
  sessions:
    - session_id: triage
      persona_id: external-persona
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let error = import_stack(
            &EmptyControlPlane,
            context,
            StackImportOptions {
                resources: vec!["session/triage".to_string()],
                allow_secret_env: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("KheishStack import refused"), "{message}");
        assert!(message.contains("external-persona"), "{message}");
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn import_rejects_explicit_resource_not_declared_in_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: hijack
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let error = import_stack(
            &EmptyControlPlane,
            context,
            StackImportOptions {
                resources: vec!["session/victim".to_string()],
                allow_secret_env: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains(
                "stack import resource `session/victim` is not declared in this KheishStack manifest"
            ),
            "{message}"
        );
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn import_rejects_external_connector_policy_persona_before_ledger_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: import-external-connector-persona
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          persona_id: external-persona
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let error = import_stack(
            &EmptyControlPlane,
            context,
            StackImportOptions {
                resources: vec!["connector/http/ingress".to_string()],
                allow_secret_env: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("KheishStack import refused"), "{message}");
        assert!(message.contains("external-persona"), "{message}");
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn import_rejects_excess_explicit_resources_before_ledger_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: import-resource-cap
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let resources = (0..=STACK_MAX_RESOURCE_COUNT)
            .map(|index| format!("session/import-resource-cap-{index}"))
            .collect();

        let error = import_stack(
            &EmptyControlPlane,
            context,
            StackImportOptions {
                resources,
                allow_secret_env: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("explicit resources"), "{message}");
        assert!(message.contains("resource limit"), "{message}");
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn validate_rejects_file_refs_through_daemon_api_context() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: self-contained-only
spec:
  personas:
    - persona_id: reviewer
      display_name: Reviewer
      soul_file: reviewer.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "file reference reviewer.md is not allowed through the daemon Stack API"
        ));
    }

    #[tokio::test]
    async fn apply_rejects_flow_start_file_refs_through_daemon_api_context() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: flow-file-self-contained
spec:
  schedules:
    - name: flow-file-ref
      target_session_id: reviewer
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
          digest: digest
        request:
          content_file: prompt.md
"#;
        let context = StackContext::from_manifest(raw, PathBuf::from("."), None, false).unwrap();

        let error = apply_stack(
            &EmptyControlPlane,
            context,
            StackApplyOptions {
                dry_run: true,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("spec.schedules[flow-file-ref].flow_start.request.content_file"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_opaque_agent_templates_in_v1alpha1() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: no-opaque-noop
spec:
  agent_templates:
    - name: custom
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.agent_templates is not supported by KheishStack v1alpha1"
        ));
    }

    #[test]
    fn manifest_source_rejects_yaml_anchor_and_alias_before_parse() {
        let anchor = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata: &metadata
  name: anchor-stack
spec: {}
"#;
        let alias = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: alias-stack
spec:
  agent_templates: *templates
"#;
        let tagged_anchor = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata: !<tag:yaml.org,2002:map> &metadata
  name: tagged-anchor-stack
spec: {}
"#;
        let sequence_block_scalar_sibling = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: sequence-block-sibling-anchor
spec:
  personas:
    - soul: |
        harmless markdown
      persona_id: reviewer
      display_name: Reviewer
      capability_scope: &closed
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      metadata: *closed
"#;

        for raw in [anchor, alias, tagged_anchor, sequence_block_scalar_sibling] {
            let error = StackContext::from_manifest(raw, PathBuf::from("."), None, true)
                .expect_err("anchor/alias should be rejected before serde_yaml parse");
            let message = format!("{error:#}");
            assert!(
                message.contains("anchors and aliases are disabled"),
                "{message}"
            );
        }
    }

    #[test]
    fn manifest_source_allows_anchor_like_text_inside_strings_and_block_scalars() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: prose-stack
  labels:
    glob: "*quoted"
    note: see *literal as prose
spec:
  personas:
    - persona_id: prose
      display_name: Prose
      soul: |2
          * This is markdown, not a YAML alias.
          & This is prose, not a YAML anchor.
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        StackContext::from_manifest(raw, PathBuf::from("."), None, true)
            .expect("quoted and block-scalar anchor-like prose should be allowed");
    }

    #[tokio::test]
    async fn validate_rejects_excess_stack_resources() {
        let mut raw = String::from(
            r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: resource-cap
spec:
  verification:
"#,
        );
        for index in 0..=STACK_MAX_RESOURCE_COUNT {
            raw.push_str(&format!(
                "    - name: secret-{index}\n      type: secret_exists\n      slot: stack.secret.{index}\n"
            ));
        }

        let validation = validate_stack_context(&context(&raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(&validation, "resource limit"));
    }

    #[tokio::test]
    async fn validate_rejects_duplicate_mcp_requirements() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: duplicate-mcp-requires
spec:
  requires:
    mcp:
      servers: ["github", "github"]
      tools: ["mcp__github__get_issue", ""]
"#;

        let validation = validate_stack_context(&context(raw)).await.unwrap();

        assert!(!validation.valid);
        assert!(errors_contain(
            &validation,
            "spec.requires.mcp.servers contains duplicate value github"
        ));
        assert!(errors_contain(
            &validation,
            "spec.requires.mcp.tools cannot be empty"
        ));
    }

    #[tokio::test]
    async fn plan_blocks_when_required_mcp_surface_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: missing-mcp
spec:
  requires:
    mcp:
      servers: ["github"]
      tools: ["mcp__github__create_pull_request"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let plan = build_plan(
            &RuntimeMcpControlPlane::with_mcp(&[], &[]),
            &context,
            false,
            false,
        )
        .await
        .unwrap();

        assert!(!plan.valid);
        assert!(plan.errors.iter().any(|error| {
            error.contains("required MCP server `github` is not satisfied in the daemon runtime")
        }));
        assert!(plan.errors.iter().any(|error| {
            error.contains("required MCP tool `mcp__github__create_pull_request` is not active")
        }));
        assert!(
            plan.actions
                .iter()
                .any(|action| action.resource_type == "mcp_server"
                    && action.resource_id == "github"
                    && action.operation == "blocked")
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.resource_type == "mcp_tool"
                    && action.resource_id == "mcp__github__create_pull_request"
                    && action.operation == "blocked")
        );

        let verification = verify_stack(&RuntimeMcpControlPlane::with_mcp(&[], &[]), &context)
            .await
            .unwrap();
        assert!(!verification.valid);
        assert!(verification.checks.iter().any(|check| {
            check.target == "mcp_tool/mcp__github__create_pull_request"
                && !check.ok
                && check.detail.contains("missing from the active MCP surface")
        }));
    }

    #[tokio::test]
    async fn plan_checks_detailed_mcp_server_requirements() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: credentialed-mcp
spec:
  requires:
    mcp:
      servers:
        - name: github
          source: codex_config
          uses_credentials: true
          credential_secret_refs: ["mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN"]
      tools: ["mcp__github__create_pull_request"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let uncredentialed = RuntimeMcpControlPlane::with_mcp_snapshot(
            vec![kheish_mcp::McpServerSnapshot {
                server: "github".to_string(),
                source: Some("codex_config".to_string()),
                connected: true,
                tools: vec!["mcp__github__create_pull_request".to_string()],
                ..Default::default()
            }],
            &["mcp__github__create_pull_request"],
        );

        let plan = build_plan(&uncredentialed, &context, false, false)
            .await
            .unwrap();
        assert!(!plan.valid);
        assert!(plan.errors.iter().any(|error| {
            error.contains("required MCP server `github` is not satisfied")
                && error.contains("uses_credentials=true")
        }));

        let credentialed = RuntimeMcpControlPlane::with_mcp_snapshot(
            vec![kheish_mcp::McpServerSnapshot {
                server: "github".to_string(),
                source: Some("codex_config".to_string()),
                uses_credentials: true,
                credential_secret_refs: vec!["mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
                connected: true,
                tools: vec!["mcp__github__create_pull_request".to_string()],
                ..Default::default()
            }],
            &["mcp__github__create_pull_request"],
        );

        let plan = build_plan(&credentialed, &context, false, false)
            .await
            .unwrap();
        assert!(plan.valid, "{:?}", plan.errors);
    }

    #[tokio::test]
    async fn plan_and_verify_accept_active_mcp_requirements() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: active-mcp
spec:
  requires:
    mcp:
      servers: ["github"]
      tools: ["mcp__github__create_pull_request"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let control =
            RuntimeMcpControlPlane::with_mcp(&["github"], &["mcp__github__create_pull_request"]);

        let plan = build_plan(&control, &context, false, false).await.unwrap();
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(
            plan.actions
                .iter()
                .any(|action| action.resource_type == "mcp_server"
                    && action.resource_id == "github"
                    && action.operation == "noop")
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.resource_type == "mcp_tool"
                    && action.resource_id == "mcp__github__create_pull_request"
                    && action.operation == "noop")
        );

        let verification = verify_stack(&control, &context).await.unwrap();
        assert!(verification.valid, "{:?}", verification.checks);
        assert!(
            verification
                .checks
                .iter()
                .any(|check| check.kind == "requirement"
                    && check.target == "mcp_server/github"
                    && check.ok)
        );
        assert!(
            verification
                .checks
                .iter()
                .any(|check| check.kind == "requirement"
                    && check.target == "mcp_tool/mcp__github__create_pull_request"
                    && check.ok)
        );
    }

    #[tokio::test]
    async fn linear_github_feature_loop_example_plans_with_declared_mcp_surface() {
        let _guard = crate::debug::debug_capture_env_lock();
        let _env_guard = FeatureLoopEnvGuard::set();
        let temp = tempfile::tempdir().expect("tempdir");
        let context = linear_github_feature_loop_context(temp.path());
        let resolved = ResolvedStack::from_context(&context).await.unwrap();
        let fixture = linear_github_feature_loop_mcp_fixture();
        fixture.assert_valid();
        assert_feature_loop_manifest_mcp_surface(&resolved, &fixture);
        let control = linear_github_feature_loop_control(&fixture.runtime_tools);

        let plan = build_plan(&control, &context, false, true).await.unwrap();

        assert!(plan.valid, "{:?}", plan.errors);
        assert_eq!(control.playbook_validate_posts(), 1);
        assert_feature_loop_secret_contract(&resolved, &plan);
        assert_feature_loop_mcp_contract(&resolved, &plan, &fixture.required_tools);
        assert_feature_loop_persona_session_contract(&resolved, &fixture.required_tools);
        assert_feature_loop_playbook_contract(&resolved, &plan);
        assert_feature_loop_schedule_contract(&resolved, &plan);
        assert_feature_loop_prompt_policy_contract(&resolved);
        assert_feature_loop_verification_contract(&resolved, &plan);
        assert_feature_loop_plan_action_set(&plan, &fixture.required_tools);

        let report = apply_stack(
            &control,
            linear_github_feature_loop_context(temp.path()),
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: true,
                prune: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(control.playbook_validate_posts(), 2);
        assert_feature_loop_apply_report(&report);

        let verification = verify_stack(&control, &linear_github_feature_loop_context(temp.path()))
            .await
            .unwrap();
        assert!(verification.valid, "{:#?}", verification.checks);
    }

    #[tokio::test]
    async fn linear_github_feature_loop_example_fails_closed_on_missing_mcp_tool() {
        let _guard = crate::debug::debug_capture_env_lock();
        let _env_guard = FeatureLoopEnvGuard::set();
        let temp = tempfile::tempdir().expect("tempdir");
        let context = linear_github_feature_loop_context(temp.path());
        let resolved = ResolvedStack::from_context(&context).await.unwrap();
        let fixture = linear_github_feature_loop_mcp_fixture();
        fixture.assert_valid();
        assert_feature_loop_manifest_mcp_surface(&resolved, &fixture);
        for missing_tool in &fixture.required_tools {
            let mut tools = fixture.runtime_tools.clone();
            tools.retain(|tool| tool != missing_tool);
            let control = linear_github_feature_loop_control(&tools);

            let plan = build_plan(&control, &context, false, true).await.unwrap();

            assert!(
                !plan.valid,
                "{missing_tool} unexpectedly produced a valid plan"
            );
            assert!(plan.errors.iter().any(|error| {
                error.contains(&format!("required MCP tool `{missing_tool}` is not active"))
            }));
            assert_action(&plan, "requirements", "mcp_tool", missing_tool, "blocked");
        }
    }

    #[tokio::test]
    async fn linear_github_feature_loop_example_fails_on_stale_manifest_tool_name() {
        let _guard = crate::debug::debug_capture_env_lock();
        let _env_guard = FeatureLoopEnvGuard::set();
        let temp = tempfile::tempdir().expect("tempdir");
        let (root, raw) = linear_github_feature_loop_raw();
        let stale_raw = raw.replace("mcp__linear__save_issue", "mcp__linear__update_issue");
        let context = linear_github_feature_loop_context_from_raw(&stale_raw, root, temp.path());
        let fixture = linear_github_feature_loop_mcp_fixture();
        fixture.assert_valid();
        let control = linear_github_feature_loop_control(&fixture.runtime_tools);

        let plan = build_plan(&control, &context, false, true).await.unwrap();

        assert!(!plan.valid);
        assert!(plan.errors.iter().any(|error| {
            error.contains("required MCP tool `mcp__linear__update_issue` is not active")
        }));
        assert_action(
            &plan,
            "requirements",
            "mcp_tool",
            "mcp__linear__update_issue",
            "blocked",
        );
    }

    struct FeatureLoopEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl FeatureLoopEnvGuard {
        fn set() -> Self {
            let previous = FEATURE_LOOP_ENVS
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in FEATURE_LOOP_ENVS {
                unsafe {
                    std::env::set_var(name, value);
                }
            }
            Self(previous)
        }
    }

    impl Drop for FeatureLoopEnvGuard {
        fn drop(&mut self) {
            for (name, previous) in self.0.drain(..) {
                match previous {
                    Some(value) => unsafe {
                        std::env::set_var(name, value);
                    },
                    None => unsafe {
                        std::env::remove_var(name);
                    },
                }
            }
        }
    }

    const FEATURE_LOOP_ENVS: &[(&str, &str)] = &[
        ("LINEAR_API_KEY", "linear-test-token"),
        ("GITHUB_PERSONAL_ACCESS_TOKEN", "github-test-token"),
    ];

    const EXPECTED_FEATURE_LOOP_INTAKE_PROMPT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/stacks/linear-github-feature-loop/intake.md"
    ));

    const EXPECTED_FEATURE_LOOP_REVIEW_PROMPT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/stacks/linear-github-feature-loop/review-followup.md"
    ));

    const EXPECTED_FEATURE_LOOP_PERSONA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/stacks/linear-github-feature-loop/persona.md"
    ));

    fn linear_github_feature_loop_context(state_root: &std::path::Path) -> StackContext {
        let (root, raw) = linear_github_feature_loop_raw();
        linear_github_feature_loop_context_from_raw(&raw, root, state_root)
    }

    fn linear_github_feature_loop_raw() -> (PathBuf, String) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/stacks/linear-github-feature-loop")
            .canonicalize()
            .expect("example stack path");
        let raw =
            std::fs::read_to_string(root.join("Kheishfile.yaml")).expect("read example Kheishfile");
        (root, raw)
    }

    fn linear_github_feature_loop_context_from_raw(
        raw: &str,
        root: PathBuf,
        state_root: &std::path::Path,
    ) -> StackContext {
        let mut context =
            StackContext::from_manifest(raw, root, Some(state_root.to_path_buf()), true).unwrap();
        context.allow_file_refs = true;
        context
    }

    fn linear_github_feature_loop_control(tools: &[String]) -> RuntimeMcpControlPlane {
        let tool_refs = tools.iter().map(String::as_str).collect::<Vec<_>>();
        RuntimeMcpControlPlane::with_mcp_snapshot(
            vec![
                kheish_mcp::McpServerSnapshot {
                    server: "github".to_string(),
                    source: Some("codex_config".to_string()),
                    uses_credentials: true,
                    credential_secret_refs: vec![
                        "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    ],
                    connected: true,
                    tools: tools
                        .iter()
                        .filter(|tool| tool.starts_with("mcp__github__"))
                        .cloned()
                        .collect(),
                    ..Default::default()
                },
                kheish_mcp::McpServerSnapshot {
                    server: "linear".to_string(),
                    source: Some("built_in_catalog".to_string()),
                    catalog_entry_id: Some("linear".to_string()),
                    uses_credentials: true,
                    credential_secret_refs: vec!["mcp.linear.LINEAR_API_KEY".to_string()],
                    connected: true,
                    tools: tools
                        .iter()
                        .filter(|tool| tool.starts_with("mcp__linear__"))
                        .cloned()
                        .collect(),
                    ..Default::default()
                },
            ],
            &tool_refs,
        )
    }

    struct FeatureLoopMcpFixture {
        runtime_tools: Vec<String>,
        required_tools: Vec<String>,
    }

    impl FeatureLoopMcpFixture {
        fn assert_valid(&self) {
            let runtime = self.runtime_tools.iter().cloned().collect::<BTreeSet<_>>();
            for tool in &self.required_tools {
                assert!(
                    runtime.contains(tool),
                    "fixture is missing required tool {tool}"
                );
            }
            assert!(
                !runtime.contains("mcp__linear__update_issue"),
                "fixture must catch stale Linear update_issue declarations"
            );
        }
    }

    fn linear_github_feature_loop_mcp_fixture() -> FeatureLoopMcpFixture {
        let required_tools = vec![
            "mcp__github__get_me",
            "mcp__github__search_code",
            "mcp__github__search_pull_requests",
            "mcp__github__get_file_contents",
            "mcp__github__list_branches",
            "mcp__github__create_branch",
            "mcp__github__create_or_update_file",
            "mcp__github__push_files",
            "mcp__github__create_pull_request",
            "mcp__github__update_pull_request",
            "mcp__github__list_pull_requests",
            "mcp__github__pull_request_read",
            "mcp__github__add_reply_to_pull_request_comment",
            "mcp__github__add_issue_comment",
            "mcp__linear__get_issue",
            "mcp__linear__list_comments",
            "mcp__linear__list_issues",
            "mcp__linear__list_issue_statuses",
            "mcp__linear__list_projects",
            "mcp__linear__list_teams",
            "mcp__linear__save_comment",
            "mcp__linear__save_issue",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
        let runtime_tools = vec![
            "mcp__github__add_issue_comment",
            "mcp__github__add_reply_to_pull_request_comment",
            "mcp__github__create_branch",
            "mcp__github__create_or_update_file",
            "mcp__github__create_pull_request",
            "mcp__github__get_file_contents",
            "mcp__github__get_me",
            "mcp__github__list_branches",
            "mcp__github__list_pull_requests",
            "mcp__github__push_files",
            "mcp__github__pull_request_read",
            "mcp__github__search_code",
            "mcp__github__search_pull_requests",
            "mcp__github__update_pull_request",
            "mcp__linear__get_issue",
            "mcp__linear__get_profile",
            "mcp__linear__list_comments",
            "mcp__linear__list_issues",
            "mcp__linear__list_issue_statuses",
            "mcp__linear__list_projects",
            "mcp__linear__list_teams",
            "mcp__linear__save_comment",
            "mcp__linear__save_issue",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
        FeatureLoopMcpFixture {
            runtime_tools,
            required_tools,
        }
    }

    fn assert_feature_loop_secret_contract(resolved: &ResolvedStack, plan: &StackPlan) {
        let secrets = resolved
            .secrets
            .iter()
            .map(|secret| (secret.slot.as_str(), secret))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            secrets.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN",
                "mcp.linear.LINEAR_API_KEY",
            ])
        );
        let linear = secrets
            .get("mcp.linear.LINEAR_API_KEY")
            .expect("linear secret requirement");
        assert_eq!(linear.provider, kheish_auth::AuthProvider::Generic);
        assert_eq!(linear.value_env.as_deref(), Some("LINEAR_API_KEY"));
        let github = secrets
            .get("mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN")
            .expect("github secret requirement");
        assert_eq!(github.provider, kheish_auth::AuthProvider::Generic);
        assert_eq!(
            github.value_env.as_deref(),
            Some("GITHUB_PERSONAL_ACCESS_TOKEN")
        );
        assert_action(
            plan,
            "secrets",
            "secret",
            "mcp.linear.LINEAR_API_KEY",
            "create",
        );
        assert_action(
            plan,
            "secrets",
            "secret",
            "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN",
            "create",
        );
    }

    fn assert_feature_loop_manifest_mcp_surface(
        resolved: &ResolvedStack,
        fixture: &FeatureLoopMcpFixture,
    ) {
        let expected_tools = fixture
            .required_tools
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            resolved
                .mcp_requirements
                .tools
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_tools
        );
        assert_eq!(
            resolved.personas[0]
                .capability_scope
                .as_ref()
                .expect("persona capability scope")
                .mcp_tool_allow
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_tools
        );
        assert_eq!(
            resolved.sessions[0]
                .capability_scope
                .as_ref()
                .expect("session capability scope")
                .mcp_tool_allow
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_tools
        );
    }

    fn assert_feature_loop_mcp_contract(
        resolved: &ResolvedStack,
        plan: &StackPlan,
        tools: &[String],
    ) {
        let expected_tools = tools.iter().cloned().collect::<BTreeSet<_>>();
        let actual_tools = resolved
            .mcp_requirements
            .tools
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_tools, expected_tools);
        let servers = resolved
            .mcp_requirements
            .servers
            .iter()
            .map(|server| (server.name.as_str(), server))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            servers.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from(["github", "linear"])
        );
        let github = servers.get("github").expect("github MCP requirement");
        assert_eq!(github.source.as_deref(), Some("codex_config"));
        assert_eq!(github.catalog_entry_id.as_deref(), None);
        assert_eq!(github.uses_credentials, Some(true));
        assert_eq!(
            github.credential_secret_refs,
            vec!["mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN".to_string()]
        );
        let linear = servers.get("linear").expect("linear MCP requirement");
        assert_eq!(linear.source.as_deref(), Some("built_in_catalog"));
        assert_eq!(linear.catalog_entry_id.as_deref(), Some("linear"));
        assert_eq!(linear.uses_credentials, Some(true));
        assert_eq!(
            linear.credential_secret_refs,
            vec!["mcp.linear.LINEAR_API_KEY".to_string()]
        );
        assert_action(plan, "requirements", "mcp_server", "github", "noop");
        assert_action(plan, "requirements", "mcp_server", "linear", "noop");
        for tool in tools {
            assert_action(plan, "requirements", "mcp_tool", tool, "noop");
        }
    }

    fn assert_feature_loop_persona_session_contract(resolved: &ResolvedStack, tools: &[String]) {
        assert_eq!(resolved.personas.len(), 1);
        let persona = &resolved.personas[0];
        assert_eq!(persona.persona_id, "feature-pr-operator-v013");
        assert_eq!(persona.display_name, "Feature PR Operator");
        assert_eq!(
            persona.soul.trim_end(),
            EXPECTED_FEATURE_LOOP_PERSONA.trim_end()
        );
        assert_eq!(persona.metadata, Value::Null);
        assert_scope_allows_exact_mcp(
            persona
                .capability_scope
                .as_ref()
                .expect("persona capability scope"),
            tools,
        );
        assert!(persona.default_skills.is_empty());

        assert_eq!(resolved.sessions.len(), 1);
        let session = &resolved.sessions[0];
        assert_eq!(session.session_id, "feature-pr-loop-v015");
        assert_eq!(session.thread_id, None);
        assert_eq!(
            session.persona_id.as_deref(),
            Some("feature-pr-operator-v013")
        );
        assert_eq!(
            session.reply_targets,
            Some(vec![crate::SessionReplyTargetRequest::Telegram {
                connector: "feature-loop-operator-telegram".to_string(),
                chat_id: 123456789,
                message_thread_id: None,
                reply_to_message_id: None,
            }])
        );
        let operator = session.operator.as_ref().expect("session operator config");
        assert!(operator.enabled);
        assert_eq!(operator.display_name.as_deref(), Some("Project operator"));
        assert_eq!(
            operator.communication_style.as_deref(),
            Some("concise and human")
        );
        assert!(operator.allow_notify);
        assert!(operator.allow_questions);
        assert_scope_allows_exact_mcp(
            session
                .capability_scope
                .as_ref()
                .expect("session capability scope"),
            tools,
        );
        let credentials = session
            .credential_scope
            .as_ref()
            .expect("session credential scope");
        assert_eq!(credentials.route_allow, vec!["openai".to_string()]);
        assert!(credentials.route_deny.is_empty());
        assert_eq!(
            credentials.connector_allow,
            vec!["feature-loop-operator-telegram".to_string()]
        );
        assert!(credentials.connector_deny.is_empty());
        assert_eq!(
            credentials.connector_credential_allow,
            vec!["feature-loop-operator-telegram:bot_token".to_string()]
        );
        assert!(credentials.connector_credential_deny.is_empty());
        assert_eq!(
            credentials
                .mcp_server_allow
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            feature_loop_server_set()
        );
        assert!(credentials.mcp_server_deny.is_empty());
        let route_policy = session.route_policy.as_ref().expect("session route policy");
        assert_eq!(route_policy.provider.as_deref(), Some("openai"));
        assert_feature_loop_generation(
            route_policy
                .generation
                .as_ref()
                .expect("session route generation"),
        );
    }

    fn assert_feature_loop_playbook_contract(resolved: &ResolvedStack, plan: &StackPlan) {
        assert_eq!(resolved.playbooks.len(), 1);
        let playbook = &resolved.playbooks[0];
        assert_eq!(
            playbook.manifest.playbook_id,
            "linear-github-feature-pr-loop"
        );
        assert_eq!(playbook.manifest.version, "0.1.14");
        assert_eq!(playbook.manifest.title, "Linear to GitHub Feature PR Loop");
        assert_eq!(
            playbook.manifest.objective,
            "Convert eligible Linear feature tickets into reviewed GitHub pull requests."
        );
        assert_eq!(
            playbook.manifest.description.as_deref(),
            Some(
                "Generic operator playbook for scheduled Linear intake and GitHub review follow-up."
            )
        );
        assert!(playbook.manifest.preconditions.is_empty());
        assert!(playbook.manifest.required_evidence.is_empty());
        assert_eq!(
            playbook.manifest.scopes,
            crate::PlaybookScopePolicy::default()
        );
        assert_eq!(
            playbook.manifest.runtime_defaults.provider.as_deref(),
            Some("openai")
        );
        assert_eq!(
            playbook.manifest.runtime_defaults.model.as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(
            playbook.manifest.metadata,
            json!({
                "owner": "examples",
                "workflow": "linear-github-feature-loop"
            })
        );
        let inputs = playbook
            .manifest
            .inputs
            .iter()
            .map(|input| (input.name.as_str(), input))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            inputs.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from(["linear_team_key", "project", "repository"])
        );
        assert_playbook_input(
            inputs.get("project").copied(),
            "Linear project or team scope to inspect.",
            false,
        );
        assert_playbook_input(
            inputs.get("repository").copied(),
            "GitHub repository scope.",
            false,
        );
        assert_playbook_input(
            inputs.get("linear_team_key").copied(),
            "Optional Linear team key to use when the deployment scope is a team rather than a Linear Project.",
            false,
        );
        let roles = playbook
            .manifest
            .roles
            .iter()
            .map(|role| (role.role_id.as_str(), role.purpose.as_str()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            roles,
            BTreeMap::from([
                (
                    "coordinator",
                    "Select safe work, coordinate subagents, and maintain Linear/GitHub state.",
                ),
                (
                    "implementer",
                    "Make code changes in an isolated worktree and run tests.",
                ),
                (
                    "reviewer",
                    "Review diff, tests, and acceptance criteria until the PR can leave draft status.",
                ),
            ])
        );
        let phases = playbook
            .manifest
            .phases
            .iter()
            .map(|phase| (phase.phase_id.as_str(), phase))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            phases.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from(["discover", "implement", "plan", "publish", "review"])
        );
        assert_playbook_phase(
            phases.get("discover").copied(),
            "Identify eligible Linear issues or complete GitHub feedback envelopes.",
            &[
                "An active session goal is checked before scanning for new work.",
                "Existing active goals are continued instead of selecting a different ticket or PR.",
                "A newly selected ticket or PR gets a session goal before GitHub or Linear mutation.",
                "Exactly one ticket or PR is selected for the run.",
                "Fresh Linear intake scans unmarked open issues in the configured project or team before concluding there is no work.",
                "When `linear_team_key` is present or the configured project resolves as a team, issues are listed from the resolved Linear team.",
                "Linear intake does not maintain unrelated existing workflow PRs; GitHub follow-up owns those.",
                "Daemon state files and prior run logs are not scanned to discover current flow metadata.",
                "Workflow footers are used for recovery and deduplication, not as the only selector for new Linear work.",
                "Existing PRs are detected before new PR creation.",
                "GitHub feedback is inventoried across review summaries, inline threads, replies, and top-level PR comments before action.",
                "Previously blocked Linear issues without PRs can be recovered by a later run.",
                "Unsafe or ambiguous items are reported instead of auto-mutated.",
            ],
        );
        assert_playbook_phase(
            phases.get("plan").copied(),
            "Analyze ticket requirements and affected source code with a high-reasoning subagent.",
            &[
                "The plan names files, risks, and focused tests.",
                "Subagent prompts are compact and do not include the full coordinator transcript.",
            ],
        );
        assert_playbook_phase(
            phases.get("implement").copied(),
            "Implement the change in an isolated worktree.",
            &[
                "The implementation is limited to the ticket scope.",
                "Project-native dev/test entrypoints such as Docker Compose, Makefile, package scripts, CI workflow commands, or devcontainers are inspected before host missing tools are reported as a test blocker.",
                "Focused tests or a clear blocked reason are recorded.",
            ],
        );
        assert_playbook_phase(
            phases.get("review").copied(),
            "Obtain an internal xhigh review score after a durable draft PR exists.",
            &[
                "Review evidence includes diff scope, tests, and remaining risk.",
                "Follow-up reviews treat a GitHub review as an envelope instead of acting on one inline fragment.",
                "Follow-up runs recheck stale blocked test evidence before preserving a tests blocker.",
                "Scores below 10/10 keep the PR draft and trigger another fix iteration or a blocked report.",
                "Reviewer timeout or context-limit failure preserves the latest completed score and does not spawn unbounded review work.",
                "The root run does not intentionally finish while spawned subagents remain running.",
            ],
        );
        assert_playbook_phase(
            phases.get("publish").copied(),
            "Open or update the GitHub PR and update the Linear ticket.",
            &[
                "A coherent ticket-scoped patch is published as a draft PR before the final 10/10 gate.",
                "The PR references the Linear ticket.",
                "The PR and Linear ticket record PR URL, tests, review score, and blockers.",
                "The PR and Linear ticket record handled GitHub feedback watermarks when feedback is processed.",
                "Machine-readable footers never contain placeholder PR URLs after a PR exists.",
            ],
        );
        assert_eq!(
            playbook.manifest.acceptance_criteria,
            vec![
                "Each root run processes at most one ticket or PR.".to_string(),
                "Safe ticket-scoped work leaves the session goal active until the daemon continuation completes it.".to_string(),
                "Goals are completed only after current tests, a 10/10 internal review, updated GitHub/Linear footers, and terminal subagents.".to_string(),
                "Goals are paused only for durable human or external blockers.".to_string(),
                "No duplicate PR is created for an issue with an active workflow PR.".to_string(),
                "Every automatic code mutation has test evidence or an explicit test blocker, plus xhigh review evidence when available.".to_string(),
                "Missing host runtimes are not enough to mark tests blocked until repository-provided Docker Compose or other project-native test commands have been tried or ruled out.".to_string(),
                "Follow-up runs do not preserve a stale `Tests:` blocker without rechecking current project-native test entrypoints.".to_string(),
                "Follow-up runs do not act on a single inline review fragment before checking sibling review comments, review metadata, and top-level PR comments.".to_string(),
                "Draft PR creation is allowed before a 10/10 review score; leaving draft status is not.".to_string(),
                "Subagents report blockers to the coordinator instead of asking user-facing clarification questions.".to_string(),
                "The workflow stops rather than guessing when human product judgment is required."
                    .to_string(),
            ]
        );
        assert_eq!(
            playbook.manifest.evidence_expectations,
            vec![
                "The root scheduled run id is available from the Flow projection.".to_string(),
                "Reviewer subagent evidence is recorded in the run output or linked PR/Linear comments.".to_string(),
                "GitHub feedback inventory evidence is recorded before feedback-driven mutation."
                    .to_string(),
            ]
        );
        assert!(!playbook.manifest.tools.enforce);
        assert!(playbook.manifest.tools.allow.is_empty());
        assert!(playbook.manifest.tools.deny.is_empty());
        let publish = playbook.publish.as_ref().expect("playbook publish spec");
        assert_eq!(publish.status, crate::PlaybookReleaseStatus::Active);
        assert_eq!(publish.evidence_refs.len(), 1);
        assert_eq!(publish.evidence_refs[0].kind, "manual");
        assert_eq!(publish.evidence_refs[0].id, "stack-example-reviewed");
        assert_eq!(
            publish.evidence_refs[0].description.as_deref(),
            Some("Example reviewed before publication.")
        );
        assert_action(
            plan,
            "playbooks",
            "playbook",
            "linear-github-feature-pr-loop/0.1.14",
            "create",
        );
        assert_action(
            plan,
            "playbooks",
            "playbook_release",
            "linear-github-feature-pr-loop/0.1.14",
            "update",
        );
    }

    fn assert_feature_loop_schedule_contract(resolved: &ResolvedStack, plan: &StackPlan) {
        assert_eq!(
            resolved
                .schedules
                .iter()
                .map(|schedule| schedule.name.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["github-review-followup-30m-v026", "linear-intake-0800-v024"])
        );
        let playbook = resolved.playbooks.first().expect("feature loop playbook");
        assert_feature_loop_schedule(
            schedule_by_name(resolved, "linear-intake-0800-v024"),
            playbook,
            "0 0 8 * * *",
            "linear-intake",
            EXPECTED_FEATURE_LOOP_INTAKE_PROMPT,
            Some(1),
        );
        assert_feature_loop_schedule(
            schedule_by_name(resolved, "github-review-followup-30m-v026"),
            playbook,
            "0 15,45 * * * *",
            "github-review-followup",
            EXPECTED_FEATURE_LOOP_REVIEW_PROMPT,
            None,
        );
        assert_action(
            plan,
            "schedules",
            "schedule",
            "linear-intake-0800-v024",
            "create",
        );
        assert_action(
            plan,
            "schedules",
            "schedule",
            "github-review-followup-30m-v026",
            "create",
        );
    }

    fn assert_feature_loop_prompt_policy_contract(resolved: &ResolvedStack) {
        let intake = schedule_by_name(resolved, "linear-intake-0800-v024")
            .request
            .flow_start
            .as_ref()
            .expect("intake schedule starts a flow")
            .request
            .content
            .as_str();
        assert!(intake.contains(
            "Recover previously blocked Kheish workflow issues that have no GitHub PR yet only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch`"
        ));
        assert!(intake.contains(
            "Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked."
        ));
        assert!(intake.contains(
            "BlockerCategory: internal-review | tests | product-judgment | credentials | ownership | unsafe | no-coherent-patch | broad-refactor | unclear-scope"
        ));
        assert!(intake.contains(
            "local Docker and Docker Compose are allowed when the repository provides them"
        ));
        assert!(intake.contains(
            "host missing tools such as `php`, `composer`, `node`, or language-specific package managers are not by themselves a test blocker"
        ));
        assert!(intake.contains("call `get_goal` before scanning Linear or GitHub work items"));
        assert!(intake.contains("leave the goal `Active` whenever the selected item still has a safe, ticket-scoped next action"));
        assert!(intake.contains("GoalId: <goal-id-or-none>"));
        assert!(intake.contains("NoProgressCount: <integer>"));
        assert!(intake.contains("LastFeedbackSeenAt: <iso8601-or-none>"));
        assert!(intake.contains("HandledFeedbackIDs: <comma-separated-ids-or-none>"));

        let followup = schedule_by_name(resolved, "github-review-followup-30m-v026")
            .request
            .flow_start
            .as_ref()
            .expect("follow-up schedule starts a flow")
            .request
            .content
            .as_str();
        assert!(followup.contains(
            "resume only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch`"
        ));
        assert!(followup.contains(
            "Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked."
        ));
        assert!(followup.contains(
            "BlockerCategory: internal-review | tests | product-judgment | credentials | ownership | unsafe | no-coherent-patch | broad-refactor | unclear-scope"
        ));
        assert!(followup.contains(
            "local Docker and Docker Compose are allowed when the repository provides them"
        ));
        assert!(followup.contains(
            "host missing tools such as `php`, `composer`, `node`, or language-specific package managers are not by themselves a test blocker"
        ));
        assert!(followup.contains(
            "If `Tests:` records blocked or unavailable test evidence, rerun the focused tests first even when there are no unresolved GitHub review comments."
        ));
        assert!(followup.contains(
            "Do not carry forward stale test blockers without rechecking the current repository and local project-native test entrypoints."
        ));
        assert!(followup.contains("call `get_goal` before scanning Linear or GitHub work items"));
        assert!(followup.contains("leave the goal `Active` whenever the selected item still has a safe, ticket-scoped next action"));
        assert!(
            followup.contains(
                "build a compact GitHub feedback inventory for each matching workflow PR"
            )
        );
        assert!(
            followup.contains("`get`, `get_reviews`, `get_review_comments`, and `get_comments`")
        );
        assert!(followup.contains(
            "If any required feedback method is unavailable or errors, record the tool failure and do not mutate the PR"
        ));
        assert!(followup.contains("Follow pagination or cursor fields when present"));
        assert!(followup.contains("record the truncation before deciding"));
        assert!(followup.contains(
            "review summaries, unresolved review threads with replies, and top-level PR comments"
        ));
        assert!(followup.contains("Never reduce a multi-line comment to only its last line"));
        assert!(followup.contains(
            "treat a GitHub review as one envelope: review summary/body when available, inline review comments, and replies"
        ));
        assert!(followup.contains(
            "update the PR body with the latest tests, review score, blockers, `LastFeedbackSeenAt`, and `HandledFeedbackIDs`"
        ));
        assert!(
            followup.contains("update the linked Linear ticket with the same footer watermarks")
        );
        assert!(followup.contains("GoalId: <goal-id-or-none>"));
        assert!(followup.contains("NoProgressCount: <integer>"));
        assert!(followup.contains("LastFeedbackSeenAt: <iso8601-or-none>"));
        assert!(followup.contains("HandledFeedbackIDs: <comma-separated-ids-or-none>"));
    }

    fn assert_feature_loop_verification_contract(resolved: &ResolvedStack, plan: &StackPlan) {
        let expected = feature_loop_probe_set();
        assert_eq!(
            resolved
                .verification
                .iter()
                .map(|probe| probe.name.as_str())
                .collect::<BTreeSet<_>>(),
            expected
        );
        for probe in expected {
            assert_action(plan, "verification", "probe", probe, "verify");
        }
        let probes = resolved
            .verification
            .iter()
            .map(|probe| (probe.name.as_str(), &probe.kind))
            .collect::<BTreeMap<_, _>>();
        assert_probe_persona(probes.get("persona").copied(), "feature-pr-operator-v013");
        assert_probe_session(probes.get("session").copied(), "feature-pr-loop-v015");
        assert_probe_schedule(
            probes.get("intake-schedule").copied(),
            "linear-intake-0800-v024",
        );
        assert_probe_schedule(
            probes.get("followup-schedule").copied(),
            "github-review-followup-30m-v026",
        );
        assert_probe_playbook(
            probes.get("playbook").copied(),
            "linear-github-feature-pr-loop",
            "0.1.14",
        );
    }

    fn assert_feature_loop_plan_action_set(plan: &StackPlan, tools: &[String]) {
        let mut expected = Vec::new();
        expected.push(action_key(
            "secrets",
            "secret",
            "mcp.linear.LINEAR_API_KEY",
            "create",
        ));
        expected.push(action_key(
            "secrets",
            "secret",
            "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN",
            "create",
        ));
        expected.push(action_key("requirements", "mcp_server", "github", "noop"));
        expected.push(action_key("requirements", "mcp_server", "linear", "noop"));
        for tool in tools {
            expected.push(action_key("requirements", "mcp_tool", tool, "noop"));
        }
        expected.push(action_key(
            "personas",
            "persona",
            "feature-pr-operator-v013",
            "create",
        ));
        expected.push(action_key(
            "sessions",
            "session",
            "feature-pr-loop-v015",
            "create",
        ));
        expected.push(action_key(
            "playbooks",
            "playbook",
            "linear-github-feature-pr-loop/0.1.14",
            "create",
        ));
        expected.push(action_key(
            "playbooks",
            "playbook_release",
            "linear-github-feature-pr-loop/0.1.14",
            "update",
        ));
        expected.push(action_key(
            "schedules",
            "schedule",
            "linear-intake-0800-v024",
            "create",
        ));
        expected.push(action_key(
            "schedules",
            "schedule",
            "github-review-followup-30m-v026",
            "create",
        ));
        for probe in feature_loop_probe_set() {
            expected.push(action_key("verification", "probe", probe, "verify"));
        }
        expected.sort();
        assert_eq!(plan_action_keys(plan), expected);
    }

    fn assert_feature_loop_apply_report(report: &StackApplyReport) {
        let verification = report.verification.as_ref().expect("apply verification");
        assert!(verification.valid, "{:#?}", verification.checks);
        assert!(
            verification.checks.iter().all(|check| check.ok),
            "{:#?}",
            verification.checks
        );
        let mut expected = vec![
            action_key("secrets", "secret", "mcp.linear.LINEAR_API_KEY", "create"),
            action_key(
                "secrets",
                "secret",
                "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN",
                "create",
            ),
            action_key("personas", "persona", "feature-pr-operator-v013", "create"),
            action_key("sessions", "session", "feature-pr-loop-v015", "create"),
            action_key(
                "playbooks",
                "playbook",
                "linear-github-feature-pr-loop/0.1.14",
                "apply",
            ),
            action_key("schedules", "schedule", "linear-intake-0800-v024", "create"),
            action_key(
                "schedules",
                "schedule",
                "github-review-followup-30m-v026",
                "create",
            ),
        ];
        expected.sort();
        assert_eq!(action_keys(&report.applied), expected);
    }

    fn assert_feature_loop_schedule(
        schedule: &ResolvedSchedule,
        playbook: &ResolvedPlaybook,
        cron_expression: &str,
        workflow: &str,
        expected_content: &str,
        max_tickets: Option<i64>,
    ) {
        assert_eq!(schedule.request.name, schedule.name);
        assert_eq!(schedule.request.target_session_id, "feature-pr-loop-v015");
        assert_eq!(schedule.request.target_agent_id, None);
        assert_eq!(schedule.request.max_executions, None);
        assert_eq!(
            schedule.request.overlap_policy,
            crate::ScheduleOverlapPolicy::Skip
        );
        assert_eq!(
            schedule.request.misfire_policy,
            crate::ScheduleMisfirePolicy::CoalesceOnce
        );
        match &schedule.request.cadence {
            crate::ScheduleCadence::Cron {
                expression,
                timezone,
            } => {
                assert_eq!(expression, cron_expression);
                assert_eq!(timezone, "Europe/Paris");
            }
            other => panic!("expected cron schedule, got {other:?}"),
        }
        assert!(schedule.request.request.is_none());
        assert!(schedule.request.observation_materialization.is_none());
        let flow_start = schedule
            .request
            .flow_start
            .as_ref()
            .expect("feature loop schedules start flows");
        assert_eq!(flow_start.flow_id, None);
        assert_eq!(flow_start.idempotency_key, None);
        assert!(flow_start.evidence_refs.is_empty());
        assert_eq!(
            flow_start.playbook_ref.playbook_id,
            playbook.manifest.playbook_id
        );
        assert_eq!(flow_start.playbook_ref.version, playbook.manifest.version);
        assert_eq!(flow_start.playbook_ref.digest, playbook.digest);
        assert_eq!(flow_start.session_id, "feature-pr-loop-v015");
        assert_eq!(flow_start.request.provider.as_deref(), Some("openai"));
        assert_eq!(flow_start.request.source_plugin, None);
        assert_eq!(flow_start.request.source_kind, None);
        assert_eq!(flow_start.request.actor_id, None);
        assert!(flow_start.request.input_items.is_empty());
        assert!(flow_start.request.attachments.is_empty());
        assert_eq!(flow_start.request.completion_requirements, None);
        assert_eq!(flow_start.request.metadata, Some(Value::Null));
        assert!(flow_start.request.binding_keys.is_empty());
        assert!(flow_start.request.reply_targets.is_empty());
        assert_eq!(flow_start.request.reply_plugin, None);
        assert_eq!(flow_start.request.reply_address, None);
        assert_feature_loop_generation(
            flow_start
                .request
                .generation
                .as_ref()
                .expect("flow start generation"),
        );
        assert_eq!(
            flow_start.request.content.trim_end(),
            expected_content.trim_end()
        );
        assert_eq!(
            flow_start.metadata.get("workflow").and_then(Value::as_str),
            Some(workflow)
        );
        assert_eq!(
            flow_start
                .metadata
                .get("max_tickets")
                .and_then(Value::as_i64),
            max_tickets
        );
        assert_eq!(
            flow_start.metadata.get("project").and_then(Value::as_str),
            Some("ExampleProject")
        );
        assert_eq!(
            flow_start
                .metadata
                .get("linear_team_key")
                .and_then(Value::as_str),
            Some("ENG")
        );
        assert_eq!(
            flow_start
                .metadata
                .get("repository")
                .and_then(Value::as_str),
            Some("example-org/example-app")
        );
        assert_eq!(
            flow_start
                .metadata
                .get("goal_token_budget")
                .and_then(Value::as_i64),
            Some(600000)
        );
        assert_eq!(
            flow_start
                .metadata
                .get("max_goal_attempts")
                .and_then(Value::as_i64),
            Some(6)
        );
        assert_eq!(
            flow_start
                .metadata
                .get("max_no_progress")
                .and_then(Value::as_i64),
            Some(2)
        );
    }

    fn assert_scope_allows_exact_mcp(scope: &kheish_types::CapabilityScope, tools: &[String]) {
        assert!(scope.skill_deny.is_empty());
        assert_eq!(
            scope.skill_allow.iter().cloned().collect::<BTreeSet<_>>(),
            string_set(["notify_operator", "ask_operator"])
        );
        assert!(scope.mcp_server_deny.is_empty());
        assert!(scope.mcp_tool_deny.is_empty());
        assert_eq!(
            scope
                .mcp_server_allow
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            feature_loop_server_set()
        );
        assert_eq!(
            scope
                .mcp_tool_allow
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            string_set(tools.iter().map(String::as_str))
        );
    }

    fn assert_feature_loop_generation(generation: &kheish_runtime::ModelGenerationConfig) {
        assert_eq!(generation.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(generation.max_output_tokens, Some(6000));
        assert_eq!(
            generation
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort),
            Some(kheish_runtime::ReasoningEffort::Medium)
        );
    }

    fn assert_playbook_input(
        input: Option<&crate::PlaybookInputSpec>,
        expected_description: &str,
        expected_required: bool,
    ) {
        let input = input.expect("playbook input");
        assert_eq!(input.description, expected_description);
        assert_eq!(input.required, expected_required);
    }

    fn assert_playbook_phase(
        phase: Option<&crate::PlaybookPhase>,
        expected_objective: &str,
        expected_acceptance_criteria: &[&str],
    ) {
        let phase = phase.expect("playbook phase");
        assert_eq!(phase.objective, expected_objective);
        assert!(phase.required_evidence.is_empty());
        assert_eq!(
            phase.acceptance_criteria,
            expected_acceptance_criteria
                .iter()
                .map(|criterion| (*criterion).to_string())
                .collect::<Vec<_>>()
        );
    }

    fn schedule_by_name<'a>(resolved: &'a ResolvedStack, name: &str) -> &'a ResolvedSchedule {
        resolved
            .schedules
            .iter()
            .find(|schedule| schedule.name == name)
            .unwrap_or_else(|| panic!("missing schedule {name}"))
    }

    fn assert_action(
        plan: &StackPlan,
        phase: &str,
        resource_type: &str,
        resource_id: &str,
        operation: &str,
    ) {
        assert!(
            plan.actions.iter().any(|action| {
                action.phase == phase
                    && action.resource_type == resource_type
                    && action.resource_id == resource_id
                    && action.operation == operation
            }),
            "missing action {phase}/{resource_type}/{resource_id}/{operation}: {:#?}",
            plan.actions
        );
    }

    fn plan_action_keys(plan: &StackPlan) -> Vec<(String, String, String, String)> {
        action_keys(&plan.actions)
    }

    fn action_keys(actions: &[StackAction]) -> Vec<(String, String, String, String)> {
        let mut keys = actions
            .iter()
            .map(|action| {
                action_key(
                    &action.phase,
                    &action.resource_type,
                    &action.resource_id,
                    &action.operation,
                )
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    fn action_key(
        phase: &str,
        resource_type: &str,
        resource_id: &str,
        operation: &str,
    ) -> (String, String, String, String) {
        (
            phase.to_string(),
            resource_type.to_string(),
            resource_id.to_string(),
            operation.to_string(),
        )
    }

    fn assert_probe_persona(probe: Option<&ProbeKind>, expected_persona_id: &str) {
        match probe.expect("persona probe") {
            ProbeKind::PersonaExists { persona_id } => {
                assert_eq!(persona_id, expected_persona_id);
            }
            other => panic!("expected persona probe, got {other:?}"),
        }
    }

    fn assert_probe_session(probe: Option<&ProbeKind>, expected_session_id: &str) {
        match probe.expect("session probe") {
            ProbeKind::SessionExists { session_id } => {
                assert_eq!(session_id, expected_session_id);
            }
            other => panic!("expected session probe, got {other:?}"),
        }
    }

    fn assert_probe_schedule(probe: Option<&ProbeKind>, expected_schedule_name: &str) {
        match probe.expect("schedule probe") {
            ProbeKind::ScheduleExists { name } => {
                assert_eq!(name, expected_schedule_name);
            }
            other => panic!("expected schedule probe, got {other:?}"),
        }
    }

    fn assert_probe_playbook(
        probe: Option<&ProbeKind>,
        expected_playbook_id: &str,
        expected_version: &str,
    ) {
        match probe.expect("playbook probe") {
            ProbeKind::PlaybookVersionExists {
                playbook_id,
                version,
            } => {
                assert_eq!(playbook_id, expected_playbook_id);
                assert_eq!(version, expected_version);
            }
            other => panic!("expected playbook probe, got {other:?}"),
        }
    }

    fn feature_loop_probe_set() -> BTreeSet<&'static str> {
        BTreeSet::from([
            "followup-schedule",
            "intake-schedule",
            "persona",
            "playbook",
            "session",
        ])
    }

    fn feature_loop_server_set() -> BTreeSet<String> {
        BTreeSet::from(["github".to_string(), "linear".to_string()])
    }

    fn string_set<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
        values.into_iter().map(ToOwned::to_owned).collect()
    }

    #[tokio::test]
    async fn resolved_stack_rejects_oversized_file_refs_when_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("soul.md"),
            "x".repeat(STACK_MANIFEST_BODY_LIMIT_BYTES + 1),
        )
        .expect("write soul");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: oversized-file-ref
spec:
  personas:
    - persona_id: oversized
      display_name: Oversized
      soul_file: soul.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;
        let mut context =
            StackContext::from_manifest(raw, temp.path().to_path_buf(), None, true).unwrap();
        context.allow_file_refs = true;

        let error = ResolvedStack::from_context(&context).await.unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("KheishStack file limit"), "{message}");
    }

    #[tokio::test]
    async fn resolved_stack_rejects_playbook_manifest_file_anchors_when_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("playbook.yaml"),
            r#"
playbook_id: anchored
version: "1.0.0"
title: &title Anchored
description: Demo
steps: []
"#,
        )
        .expect("write playbook");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: anchored-playbook-ref
spec:
  playbooks:
    - manifest_file: playbook.yaml
"#;
        let mut context =
            StackContext::from_manifest(raw, temp.path().to_path_buf(), None, true).unwrap();
        context.allow_file_refs = true;

        let error = ResolvedStack::from_context(&context).await.unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("anchors and aliases are disabled"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn secret_value_env_is_not_read_without_explicit_plan_permission() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: secret-env-guard
spec:
  requires:
    secrets:
      - ref: mcp.example.API_KEY
        value_env: KHEISH_STACK_TEST_ENV_SHOULD_NOT_BE_READ
"#;
        let context = context(raw);
        let resolved = ResolvedStack::from_context(&context).await.unwrap();
        let ledger = ApplyLedger::new();
        let mut plan = empty_test_plan(&context);

        add_secret_actions(
            &EmptyControlPlane,
            &context,
            &resolved,
            &ledger,
            false,
            &mut plan,
        )
        .await
        .unwrap();

        assert!(plan.errors.iter().any(|error| error.contains(
            "pass allow_secret_env=true to read the daemon environment for planning/apply"
        )));
        assert!(
            !plan
                .errors
                .iter()
                .any(|error| error.contains("failed to read environment variable")),
            "{:?}",
            plan.errors
        );
    }

    #[tokio::test]
    async fn plan_rejects_value_env_secret_when_live_slot_is_not_imported() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_UNOWNED_LIVE_SECRET_PLAN";
        unsafe {
            std::env::set_var(env_name, "new-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: unowned-live-secret-plan
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_UNOWNED_LIVE_SECRET_PLAN
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let plan = build_plan(&LiveSecretControlPlane::default(), &context, false, true)
            .await
            .unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(!plan.valid);
        assert!(
            plan.errors.iter().any(|error| {
                error.contains(
                    "resource secret/stack.test.LIVE_SECRET already exists but is not owned",
                )
            }),
            "{:?}",
            plan.errors
        );
    }

    #[tokio::test]
    async fn plan_allows_existing_required_secret_without_value_env_unowned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: required-live-secret-plan
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let plan = build_plan(&LiveSecretControlPlane::default(), &context, false, false)
            .await
            .unwrap();

        assert!(plan.valid, "{:?}", plan.errors);
        assert!(
            plan.errors
                .iter()
                .all(|error| !error.contains("already exists but is not owned")),
            "{:?}",
            plan.errors
        );
        assert!(plan.actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "verify"
        }));
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn apply_treats_existing_required_secret_without_value_env_as_prerequisite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: required-live-secret-apply
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        let report = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap();

        assert!(
            report
                .verification
                .as_ref()
                .is_some_and(|verification| verification.valid),
            "{:?}",
            report.verification
        );
        assert!(
            report
                .applied
                .iter()
                .all(|action| action.resource_type != "secret")
        );
        assert!(client.posts.lock().is_empty());
        let ledger = ApplyLedger::load_or_new(&stack_ledger_path(temp.path()))
            .await
            .unwrap();
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("secret", "stack.test.LIVE_SECRET")),
            None
        );
    }

    #[tokio::test]
    async fn apply_allows_required_secret_owned_by_another_stack_without_value_env() {
        let temp = tempfile::tempdir().expect("tempdir");
        let secret_key = ResourceKey::new("secret", "stack.test.LIVE_SECRET");
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        ledger.record_secret(
            "credential-stack",
            "stack.test.LIVE_SECRET",
            "fingerprint".to_string(),
        );
        ledger.record_resource("credential-stack", &secret_key, "fingerprint".to_string());
        ledger.save(&ledger_path).await.unwrap();

        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: dependent-stack
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        let plan = build_plan(&client, &context, false, false).await.unwrap();
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(plan.actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "verify"
        }));

        let report = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap();

        assert!(
            report
                .verification
                .as_ref()
                .is_some_and(|verification| verification.valid),
            "{:?}",
            report.verification
        );
        assert!(client.posts.lock().is_empty());
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert_eq!(
            ledger.owner_of_resource(&secret_key),
            Some("credential-stack")
        );
    }

    #[tokio::test]
    async fn apply_rejects_value_env_secret_when_live_slot_is_not_imported_before_write() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_UNOWNED_LIVE_SECRET_APPLY";
        unsafe {
            std::env::set_var(env_name, "new-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: unowned-live-secret-apply
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_UNOWNED_LIVE_SECRET_APPLY
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        let error = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: true,
                prune: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(
            message.contains("KheishStack apply refused because the plan contains errors"),
            "{message}"
        );
        assert!(client.posts.lock().is_empty());
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn import_value_env_secret_records_fingerprint_without_rewrite() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_IMPORT_SECRET_FINGERPRINT";
        unsafe {
            std::env::set_var(env_name, "imported-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: imported-live-secret
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_IMPORT_SECRET_FINGERPRINT
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        import_stack(
            &client,
            context.clone(),
            StackImportOptions {
                resources: vec!["secret/stack.test.LIVE_SECRET".to_string()],
                allow_secret_env: true,
            },
        )
        .await
        .unwrap();
        let plan = build_plan(&client, &context, false, true).await.unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(plan.actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "noop"
        }));

        let ledger = ApplyLedger::load_or_new(&stack_ledger_path(temp.path()))
            .await
            .unwrap();
        let down_actions = plan_down(&context, &ledger);
        assert!(down_actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "blocked"
        }));
    }

    #[tokio::test]
    async fn plan_updates_managed_value_env_secret_when_live_slot_changed_after_apply() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_PLAN";
        unsafe {
            std::env::set_var(env_name, "managed-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: timestamp-drift-secret-plan
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_PLAN
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        import_stack(
            &client,
            context.clone(),
            StackImportOptions {
                resources: vec!["secret/stack.test.LIVE_SECRET".to_string()],
                allow_secret_env: true,
            },
        )
        .await
        .unwrap();
        let key = ResourceKey::new("secret", "stack.test.LIVE_SECRET");
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        ledger
            .stack_mut("timestamp-drift-secret-plan")
            .resources
            .get_mut(&key.to_string())
            .unwrap()
            .last_applied_at_ms = 1;
        ledger.save(&ledger_path).await.unwrap();

        let drifted_client = LiveSecretControlPlane {
            updated_at_ms: 2,
            ..LiveSecretControlPlane::default()
        };
        let plan = build_plan(&drifted_client, &context, false, true)
            .await
            .unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(plan.actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "update"
        }));
    }

    #[tokio::test]
    async fn apply_rewrites_managed_value_env_secret_when_live_slot_changed_after_apply() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_APPLY";
        unsafe {
            std::env::set_var(env_name, "managed-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: timestamp-drift-secret-apply
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_APPLY
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        import_stack(
            &client,
            context.clone(),
            StackImportOptions {
                resources: vec!["secret/stack.test.LIVE_SECRET".to_string()],
                allow_secret_env: true,
            },
        )
        .await
        .unwrap();
        let key = ResourceKey::new("secret", "stack.test.LIVE_SECRET");
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        ledger
            .stack_mut("timestamp-drift-secret-apply")
            .resources
            .get_mut(&key.to_string())
            .unwrap()
            .last_applied_at_ms = 1;
        ledger.save(&ledger_path).await.unwrap();

        let drifted_client = LiveSecretControlPlane {
            updated_at_ms: 2,
            allow_secret_posts: true,
            ..LiveSecretControlPlane::default()
        };
        let report = apply_stack(
            &drifted_client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: true,
                prune: false,
            },
        )
        .await
        .unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(report.applied.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "update"
        }));
        assert_eq!(
            drifted_client.posts.lock().as_slice(),
            ["/v1/runtime/secrets"]
        );
        assert!(
            report
                .verification
                .as_ref()
                .is_some_and(|verification| verification.valid),
            "{:?}",
            report.verification
        );
    }

    #[tokio::test]
    async fn verify_rejects_managed_value_env_secret_changed_after_apply() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_VERIFY";
        unsafe {
            std::env::set_var(env_name, "managed-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: timestamp-drift-secret-verify
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: generic
        value_env: KHEISH_STACK_TEST_SECRET_TIMESTAMP_DRIFT_VERIFY
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        import_stack(
            &client,
            context.clone(),
            StackImportOptions {
                resources: vec!["secret/stack.test.LIVE_SECRET".to_string()],
                allow_secret_env: true,
            },
        )
        .await
        .unwrap();
        let key = ResourceKey::new("secret", "stack.test.LIVE_SECRET");
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        ledger
            .stack_mut("timestamp-drift-secret-verify")
            .resources
            .get_mut(&key.to_string())
            .unwrap()
            .last_applied_at_ms = 1;
        ledger.save(&ledger_path).await.unwrap();

        let drifted_client = LiveSecretControlPlane {
            updated_at_ms: 2,
            ..LiveSecretControlPlane::default()
        };
        let report = verify_stack(&drifted_client, &context).await.unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(!report.valid);
        assert!(report.checks.iter().any(|check| {
            check.kind == "secret"
                && check.target == "stack.test.LIVE_SECRET"
                && !check.ok
                && check.detail.contains("may have drifted")
        }));
    }

    #[tokio::test]
    async fn secret_provider_drift_is_not_hidden_by_matching_fingerprint() {
        let _guard = crate::debug::debug_capture_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let env_name = "KHEISH_STACK_TEST_SECRET_PROVIDER_DRIFT";
        unsafe {
            std::env::set_var(env_name, "same-value");
        }
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: provider-drift-secret
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: open_ai
        value_env: KHEISH_STACK_TEST_SECRET_PROVIDER_DRIFT
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = LiveSecretControlPlane::default();

        import_stack(
            &client,
            context.clone(),
            StackImportOptions {
                resources: vec!["secret/stack.test.LIVE_SECRET".to_string()],
                allow_secret_env: true,
            },
        )
        .await
        .unwrap();
        let plan = build_plan(&client, &context, false, true).await.unwrap();

        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(plan.actions.iter().any(|action| {
            action.resource_type == "secret"
                && action.resource_id == "stack.test.LIVE_SECRET"
                && action.operation == "update"
        }));
    }

    #[tokio::test]
    async fn verify_rejects_secret_provider_drift() {
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: verify-provider-drift-secret
spec:
  requires:
    secrets:
      - ref: stack.test.LIVE_SECRET
        provider: open_ai
"#;
        let context = context(raw);
        let client = LiveSecretControlPlane::default();

        let report = verify_stack(&client, &context).await.unwrap();

        assert!(!report.valid);
        assert!(report.checks.iter().any(|check| {
            check.kind == "secret"
                && check.target == "stack.test.LIVE_SECRET"
                && !check.ok
                && check.detail.contains("provider mismatch")
        }));
    }

    #[tokio::test]
    async fn apply_rejects_live_persona_created_between_plan_and_apply() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: persona-race
spec:
  personas:
    - persona_id: race-persona
      display_name: Stack Persona
      soul: Managed by stack.
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let client = RacePersonaControlPlane::default();

        let error = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains(
                "resource persona/race-persona already exists but is not owned by stack `persona-race`"
            ),
            "{message}"
        );
        assert!(client.writes.lock().is_empty());
        let ledger = ApplyLedger::load_or_new(&stack_ledger_path(temp.path()))
            .await
            .unwrap();
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("persona", "race-persona"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_persists_connector_claim_before_create_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-claim
spec:
  connectors:
    - kind: http
      name: claimed
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let client = ClaimObservedConnectorControlPlane {
            ledger_path: ledger_path.clone(),
            observed_pending_claim: parking_lot::Mutex::new(false),
            created: parking_lot::Mutex::new(false),
        };

        let report = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap();

        assert!(
            *client.observed_pending_claim.lock(),
            "connector PUT did not observe a persisted pending claim"
        );
        assert!(
            report
                .applied
                .iter()
                .any(|action| action.resource_type == "connector"
                    && action.resource_id == "http/claimed")
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        let stack = ledger.stack("connector-claim").expect("stack ledger");
        assert!(
            stack.pending_resources.is_empty(),
            "{:?}",
            stack.pending_resources
        );
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("connector", "http/claimed")),
            Some("connector-claim")
        );
    }

    #[tokio::test]
    async fn apply_rolls_back_connector_claim_when_create_if_absent_loses_race() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-race
spec:
  connectors:
    - kind: http
      name: race
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let client = RaceConnectorCreateControlPlane {
            ledger_path: ledger_path.clone(),
            observed_pending_claim: parking_lot::Mutex::new(false),
        };

        let error = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            *client.observed_pending_claim.lock(),
            "connector create-if-absent did not observe a persisted pending claim"
        );
        assert!(
            message.contains("http connector race already exists"),
            "{message}"
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("connector", "http/race"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_preserves_connector_claim_when_create_error_may_have_committed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: connector-committed
spec:
  connectors:
    - kind: http
      name: committed
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let client = CommittedThenErroredConnectorControlPlane {
            created: parking_lot::Mutex::new(false),
        };

        let error = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: false,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("preserved pending stack ownership claim"),
            "{message}"
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("connector", "http/committed")),
            Some("connector-committed")
        );
    }

    #[tokio::test]
    async fn down_preserves_pending_claims_for_reapply_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: pending-down
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        let key = ResourceKey::new("connector", "http/pending");
        ledger
            .claim_resource("pending-down", &key, "digest".to_string())
            .unwrap();
        ledger.save(&ledger_path).await.unwrap();

        let report = down_stack(&EmptyControlPlane, context, StackDownOptions { yes: true })
            .await
            .unwrap();

        assert!(report.actions.is_empty());
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert_eq!(ledger.owner_of_resource(&key), Some("pending-down"));
        assert!(
            ledger
                .stack("pending-down")
                .expect("stack ledger")
                .pending_resources
                .contains_key(&key.to_string())
        );
    }

    #[test]
    fn secret_down_actions_are_blocked() {
        let full_down = down_action_for_key(ResourceKey::new("secret", "anthropic/api_key"), true);
        let prune = down_action_for_key(ResourceKey::new("secret", "anthropic/api_key"), false);

        assert_eq!(full_down.operation, "blocked");
        assert!(full_down.reason.contains("never deletes"));
        assert_eq!(prune.operation, "blocked");
        assert!(prune.reason.contains("deleted explicitly"));
    }

    #[tokio::test]
    async fn down_executes_deletable_resources_and_retains_blocked_resources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: partial-down
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        ledger.record_resource(
            "partial-down",
            &ResourceKey::new("connector", "http/partial-down-webhook"),
            "connector-digest".to_string(),
        );
        ledger.record_resource(
            "partial-down",
            &ResourceKey::new("persona", "partial-down-persona"),
            "persona-digest".to_string(),
        );
        ledger.save(&ledger_path).await.unwrap();
        let client = RecordingControlPlane::default();

        let report = down_stack(&client, context, StackDownOptions { yes: true })
            .await
            .unwrap();

        assert!(report.executed);
        assert!(
            report.actions.iter().any(|action| {
                action.resource_type == "persona" && action.operation == "blocked"
            })
        );
        let deletes = client.deletes.lock().clone();
        assert_eq!(
            deletes,
            vec!["/v1/runtime/connectors/http/partial-down-webhook".to_string()]
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("connector", "http/partial-down-webhook"))
                .is_none()
        );
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("persona", "partial-down-persona")),
            Some("partial-down")
        );
    }

    #[tokio::test]
    async fn prune_executes_deletable_resources_and_retains_blocked_resources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: partial-prune
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        ledger.record_resource(
            "partial-prune",
            &ResourceKey::new("connector", "http/partial-prune-webhook"),
            "connector-digest".to_string(),
        );
        ledger.record_resource(
            "partial-prune",
            &ResourceKey::new("secret", "stack.e2e.PRUNE_SECRET"),
            "secret-digest".to_string(),
        );
        ledger.save(&ledger_path).await.unwrap();
        let client = RecordingControlPlane::default();

        let report = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: true,
            },
        )
        .await
        .unwrap();

        assert!(
            report
                .applied
                .iter()
                .any(|action| action.resource_type == "connector" && action.operation == "delete")
        );
        assert!(
            !report
                .applied
                .iter()
                .any(|action| action.operation == "blocked")
        );
        let deletes = client.deletes.lock().clone();
        assert_eq!(
            deletes,
            vec!["/v1/runtime/connectors/http/partial-prune-webhook".to_string()]
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("connector", "http/partial-prune-webhook"))
                .is_none()
        );
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("secret", "stack.e2e.PRUNE_SECRET")),
            Some("partial-prune")
        );
    }

    #[tokio::test]
    async fn prune_treats_terminal_schedules_as_already_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: terminal-schedule-prune
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        ledger.record_resource(
            "terminal-schedule-prune",
            &ResourceKey::new("schedule", "old-schedule"),
            "schedule-digest".to_string(),
        );
        ledger.save(&ledger_path).await.unwrap();

        let request = crate::ScheduleCreateRequest {
            name: "old-schedule".to_string(),
            target_session_id: "session-1".to_string(),
            target_agent_id: None,
            owner_session_id: None,
            owner_agent_id: None,
            created_by_run_id: None,
            cadence: crate::ScheduleCadence::Once {
                fire_at_ms: crate::now_ms() + 60_000,
            },
            max_executions: None,
            overlap_policy: crate::ScheduleOverlapPolicy::Skip,
            misfire_policy: crate::ScheduleMisfirePolicy::CoalesceOnce,
            request: Some(crate::SubmitInputRequest {
                provider: None,
                source_plugin: None,
                source_kind: None,
                actor_id: None,
                content: "noop".to_string(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                generation: None,
                completion_requirements: None,
                metadata: None,
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: None,
                reply_address: None,
            }),
            observation_materialization: None,
            flow_start: None,
        };
        let mut schedule = crate::scheduler::build_schedule_record(
            "schedule-terminal".to_string(),
            crate::now_ms(),
            request,
        )
        .unwrap()
        .view;
        schedule.status = crate::ScheduleStatus::Canceled;
        schedule.next_fire_at_ms = None;
        schedule.queued_fire_at_ms = None;

        let client = RecordingControlPlane::default();
        client.schedules.lock().push(schedule);

        let report = apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: true,
            },
        )
        .await
        .unwrap();

        assert!(
            report.applied.iter().any(|action| {
                action.resource_type == "schedule" && action.operation == "cancel"
            })
        );
        assert!(client.posts.lock().is_empty());
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("schedule", "old-schedule"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn prune_retains_non_idle_session_and_continues_other_resources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: non-idle-session-prune
spec: {}
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();
        let ledger_path = stack_ledger_path(temp.path());
        let mut ledger = ApplyLedger::new();
        ledger.record_resource(
            "non-idle-session-prune",
            &ResourceKey::new("session", "old-session"),
            "session-digest".to_string(),
        );
        ledger.record_resource(
            "non-idle-session-prune",
            &ResourceKey::new("schedule", "old-schedule"),
            "schedule-digest".to_string(),
        );
        ledger.save(&ledger_path).await.unwrap();

        let request = crate::ScheduleCreateRequest {
            name: "old-schedule".to_string(),
            target_session_id: "old-session".to_string(),
            target_agent_id: None,
            owner_session_id: None,
            owner_agent_id: None,
            created_by_run_id: None,
            cadence: crate::ScheduleCadence::Once {
                fire_at_ms: crate::now_ms() + 60_000,
            },
            max_executions: None,
            overlap_policy: crate::ScheduleOverlapPolicy::Skip,
            misfire_policy: crate::ScheduleMisfirePolicy::CoalesceOnce,
            request: Some(crate::SubmitInputRequest {
                provider: None,
                source_plugin: None,
                source_kind: None,
                actor_id: None,
                content: "noop".to_string(),
                input_items: Vec::new(),
                attachments: Vec::new(),
                generation: None,
                completion_requirements: None,
                metadata: None,
                binding_keys: Vec::new(),
                reply_targets: Vec::new(),
                reply_plugin: None,
                reply_address: None,
            }),
            observation_materialization: None,
            flow_start: None,
        };
        let schedule = crate::scheduler::build_schedule_record(
            "schedule-active".to_string(),
            crate::now_ms(),
            request,
        )
        .unwrap()
        .view;
        let client = RecordingControlPlane::default();
        client.schedules.lock().push(schedule);

        apply_stack(
            &client,
            context,
            StackApplyOptions {
                dry_run: false,
                force_restart: false,
                allow_secret_env: false,
                prune: true,
            },
        )
        .await
        .unwrap();

        let posts = client.posts.lock().clone();
        assert!(
            posts
                .iter()
                .any(|path| path == "/v1/sessions/old-session/end")
        );
        assert!(
            posts
                .iter()
                .any(|path| path == "/v1/schedules/schedule-active/cancel")
        );
        let ledger = ApplyLedger::load_or_new(&ledger_path).await.unwrap();
        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("session", "old-session")),
            Some("non-idle-session-prune")
        );
        assert!(
            ledger
                .owner_of_resource(&ResourceKey::new("schedule", "old-schedule"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn down_rejects_api_manifest_file_refs_without_touching_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: down-file-ref
spec:
  personas:
    - persona_id: reviewer
      display_name: Reviewer
      soul_file: reviewer.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;
        let context = StackContext::from_manifest(
            raw,
            PathBuf::from("."),
            Some(temp.path().to_path_buf()),
            true,
        )
        .unwrap();

        let error = down_stack(&EmptyControlPlane, context, StackDownOptions { yes: true })
            .await
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("file references are not allowed"),
            "{message}"
        );
        assert!(
            message.contains("spec.personas[reviewer].soul_file"),
            "{message}"
        );
        assert!(!stack_ledger_path(temp.path()).exists());
    }

    #[test]
    fn ledger_lock_can_be_reacquired_after_drop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger_path = temp.path().join("ledger.json");
        let lock_path = ledger_path.with_extension("lock");

        {
            let _lock = LedgerLock::acquire(&ledger_path).expect("acquire ledger lock");
            assert!(lock_path.exists());
        }

        let _lock = LedgerLock::acquire(&ledger_path).expect("reacquire ledger lock");
    }

    #[test]
    #[cfg(unix)]
    fn ledger_lock_rejects_concurrent_holder_and_leaves_lock_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger_path = temp.path().join("ledger.json");
        let lock_path = ledger_path.with_extension("lock");
        let lock = LedgerLock::acquire(&ledger_path).expect("acquire ledger lock");

        let error =
            LedgerLock::acquire(&ledger_path).expect_err("second lock holder should be rejected");
        let message = format!("{error:#}");
        assert!(message.contains("apply ledger is locked"), "{message}");

        drop(lock);
        assert!(lock_path.exists());
        let _reacquired = LedgerLock::acquire(&ledger_path).expect("reacquire after drop");
    }

    #[test]
    #[cfg(unix)]
    fn ledger_lock_rejects_symlink_lock_file_without_truncating_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger_path = temp.path().join("ledger.json");
        let lock_path = ledger_path.with_extension("lock");
        let target = temp.path().join("target.txt");
        std::fs::write(&target, "must stay intact").expect("write target");
        std::os::unix::fs::symlink(&target, &lock_path).expect("symlink lock");

        let error =
            LedgerLock::acquire(&ledger_path).expect_err("symlink lockfile should be rejected");
        let message = format!("{error:#}");

        assert!(message.contains("failed to open"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "must stay intact"
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_private_file_rejects_symlink_without_truncating_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tmp_path = temp.path().join("ledger.json.tmp-exact");
        let target = temp.path().join("target.txt");
        std::fs::write(&target, "must stay intact").expect("write target");
        std::os::unix::fs::symlink(&target, &tmp_path).expect("symlink tmp");

        let error = write_private_file(&tmp_path, b"new ledger").expect_err("symlink tmp rejected");
        let message = format!("{error:#}");

        assert!(
            message.contains("File exists")
                || message.contains("Too many levels of symbolic links"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "must stay intact"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn ledger_save_ignores_stale_fixed_tmp_symlink_without_truncating_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger_path = temp.path().join("ledger.json");
        let stale_tmp = ledger_path.with_extension("json.tmp");
        let target = temp.path().join("target.txt");
        std::fs::write(&target, "must stay intact").expect("write target");
        std::os::unix::fs::symlink(&target, &stale_tmp).expect("symlink stale tmp");

        let ledger = ApplyLedger::new();
        ledger.save(&ledger_path).await.expect("save ledger");

        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "must stay intact"
        );
        assert!(ledger_path.exists());
        assert!(
            std::fs::symlink_metadata(&stale_tmp)
                .expect("stale tmp metadata")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn connector_secret_match_compares_redacted_sources() {
        let live_secret_ref = crate::ConnectorSecretView {
            configured: true,
            source: Some("secret_ref".to_string()),
            secret_ref: Some("mcp.example.API_KEY".to_string()),
            env: None,
        };
        let desired_secret_ref = crate::ConnectorSecretInput {
            secret_ref: Some("mcp.example.API_KEY".to_string()),
            value: None,
            env: None,
        };
        assert!(connector_secret_matches(
            &live_secret_ref,
            Some(&desired_secret_ref)
        ));

        let live_env = crate::ConnectorSecretView {
            configured: true,
            source: Some("env".to_string()),
            secret_ref: None,
            env: Some("HTTP_TOKEN".to_string()),
        };
        let desired_env = crate::ConnectorSecretInput {
            secret_ref: None,
            value: None,
            env: Some("HTTP_TOKEN".to_string()),
        };
        assert!(connector_secret_matches(&live_env, Some(&desired_env)));

        let absent = crate::ConnectorSecretView {
            configured: false,
            source: None,
            secret_ref: None,
            env: None,
        };
        assert!(connector_secret_matches(&absent, None));
        assert!(!connector_secret_matches(&absent, Some(&desired_env)));
    }

    #[test]
    fn ownership_diagnostics_reject_cross_stack_and_unowned_existing_resources() {
        let mut ledger = ApplyLedger::new();
        ledger.record_resource(
            "other-stack",
            &ResourceKey::new("persona", "owned-elsewhere"),
            "digest".to_string(),
        );
        let mut plan = StackPlan {
            stack: "current".to_string(),
            ownership_id: "current-stack".to_string(),
            ledger_path: String::new(),
            valid: true,
            restart_required: false,
            actions: vec![
                StackAction::new(
                    "personas",
                    "persona",
                    "owned-elsewhere",
                    "noop",
                    "live persona exists",
                ),
                StackAction::new(
                    "personas",
                    "persona",
                    "unowned-live",
                    "update",
                    "live persona drifted",
                ),
                StackAction::new("personas", "persona", "new", "create", "new persona"),
            ],
            errors: Vec::new(),
            warnings: Vec::new(),
            summary: StackPlanSummary::default(),
        };

        add_ownership_diagnostics(&ledger, "current-stack", &mut plan);

        assert_eq!(plan.errors.len(), 2, "{:?}", plan.errors);
        assert!(plan.errors.iter().any(|error| {
            error.contains(
                "resource persona/owned-elsewhere is already owned by stack `other-stack`",
            )
        }));
        assert!(plan.errors.iter().any(|error| {
            error.contains(
                "resource persona/unowned-live already exists but is not owned by stack `current-stack`",
            )
        }));
    }

    #[test]
    fn secret_fingerprint_tracks_value_without_storing_value() {
        let _guard = crate::debug::debug_capture_env_lock();
        let env_name = "KHEISH_STACK_TEST_SECRET_FINGERPRINT";
        let ledger = ApplyLedger::new();

        unsafe {
            std::env::set_var(env_name, "first-value");
        }
        let first = secret_fingerprint(&ledger.ledger_salt, "linear/api_key", env_name).unwrap();
        let repeated = secret_fingerprint(&ledger.ledger_salt, "linear/api_key", env_name).unwrap();
        assert_eq!(first, repeated);

        unsafe {
            std::env::set_var(env_name, "rotated-value");
        }
        let rotated = secret_fingerprint(&ledger.ledger_salt, "linear/api_key", env_name).unwrap();
        assert_ne!(first, rotated);

        unsafe {
            std::env::remove_var(env_name);
        }
    }

    #[test]
    fn owner_of_resource_recognizes_legacy_secret_records() {
        let mut ledger = ApplyLedger::new();
        ledger.record_secret(
            "legacy-stack",
            "stack.test.LEGACY_SECRET",
            "fingerprint".to_string(),
        );

        assert_eq!(
            ledger.owner_of_resource(&ResourceKey::new("secret", "stack.test.LEGACY_SECRET")),
            Some("legacy-stack")
        );
    }
}
