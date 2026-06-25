//! Playbook and Flow control-plane methods implemented on [`DaemonState`].

use std::path::{Component, Path};
use std::sync::Arc;

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) fn validate_playbook(
        &self,
        request: crate::ValidatePlaybookRequest,
    ) -> crate::PlaybookValidationResult {
        self.playbook_service.validate_playbook(request)
    }

    pub(crate) async fn list_playbooks(&self, query: PlaybookListQuery) -> Vec<PlaybookView> {
        self.playbook_service.list_playbooks(query).await
    }

    pub(crate) async fn get_playbook(
        &self,
        playbook_id: &str,
        version: Option<&str>,
    ) -> Result<PlaybookView> {
        self.playbook_service
            .get_playbook(playbook_id, version)
            .await
    }

    pub(crate) async fn create_playbook(
        &self,
        request: crate::CreatePlaybookRequest,
    ) -> Result<PlaybookView> {
        self.playbook_service.create_playbook(request).await
    }

    pub(crate) async fn publish_playbook(
        &self,
        playbook_id: &str,
        request: crate::PublishPlaybookRequest,
    ) -> Result<PlaybookView> {
        self.playbook_service
            .publish_playbook(playbook_id, request)
            .await
    }

    pub(crate) async fn revoke_playbook(
        &self,
        playbook_id: &str,
        request: crate::RevokePlaybookRequest,
    ) -> Result<PlaybookView> {
        self.playbook_service
            .revoke_playbook(playbook_id, request)
            .await
    }

    pub(crate) async fn start_flow(
        self: &Arc<Self>,
        mut request: StartFlowRequest,
    ) -> Result<FlowView> {
        if contains_flow_metadata(&request.request.metadata) {
            anyhow::bail!("metadata key `{KHEISH_FLOW_METADATA_KEY}` is daemon-owned");
        }
        if !metadata_can_accept_daemon_key(&request.request.metadata) {
            anyhow::bail!("metadata must be an object when daemon metadata is attached");
        }
        self.agent_id_for_session(&request.session_id).await?;
        self.apply_flow_runtime_defaults(&mut request).await?;
        self.ensure_submit_input_request_has_payload(&request.request)?;
        self.validate_submit_input_request(&request.session_id, &request.request)
            .await?;
        let manifest = self
            .playbook_service
            .manifest_for_ref(&request.playbook_ref)
            .await?;
        self.validate_flow_start_contract(&request.session_id, &manifest)
            .await?;
        validate_start_flow_evidence_refs(&request.evidence_refs)?;
        let reservation = self.playbook_service.reserve_flow_start(&request).await?;
        let mut record = reservation.record;

        if record.run_id.is_none()
            && let Some(recovered_run_id) = self.find_flow_run_id(&record).await?
        {
            record = self
                .playbook_service
                .attach_flow_run(&record.flow_id, &recovered_run_id)
                .await?;
        }

        if reservation.should_submit_run && record.run_id.is_none() {
            let mut run_request = request.request;
            if let Err(error) = insert_daemon_metadata(
                &mut run_request.metadata,
                KHEISH_FLOW_METADATA_KEY,
                flow_correlation_metadata(&record),
            ) {
                self.playbook_service
                    .rollback_unattached_flow_start(&record.flow_id)
                    .await?;
                return Err(error);
            }
            let run = match self
                .submit_input_run_with_daemon_metadata(&record.session_id, run_request)
                .await
            {
                Ok(run) => run,
                Err(error) => {
                    if let Err(rollback_error) = self
                        .playbook_service
                        .rollback_unattached_flow_start(&record.flow_id)
                        .await
                    {
                        return Err(anyhow!(
                            "{error}; additionally failed to roll back flow {}: {rollback_error}",
                            record.flow_id
                        ));
                    }
                    return Err(error);
                }
            };
            record = self
                .playbook_service
                .attach_flow_run(&record.flow_id, &run.run_id)
                .await?;
            return self.flow_view(record).await;
        }

        self.flow_view(record).await
    }

    pub(crate) async fn list_flows(
        self: &Arc<Self>,
        query: FlowListQuery,
    ) -> Result<Vec<FlowView>> {
        let records = self.playbook_service.list_flow_records(&query).await;
        let mut views = Vec::with_capacity(records.len());
        let mut projection_context = FlowProjectionContext::default();
        for record in records {
            let view = self
                .flow_view_with_context(record, &mut projection_context)
                .await?;
            if query
                .status
                .as_ref()
                .is_none_or(|status| &view.status == status)
            {
                views.push(view);
            }
        }
        Ok(views)
    }

    pub(crate) async fn get_flow(self: &Arc<Self>, flow_id: &str) -> Result<FlowView> {
        let record = self.playbook_service.get_flow_record(flow_id).await?;
        self.flow_view(record).await
    }

    pub(crate) async fn verify_product_view_flow(
        self: &Arc<Self>,
        flow_id: &str,
        request: ProductViewFlowVerificationRequest,
    ) -> Result<ProductViewFlowVerificationVerdict> {
        let record = self.playbook_service.get_flow_record(flow_id).await?;
        let mut projection_context = FlowProjectionContext::default();
        let root_run = match record.run_id.as_deref() {
            Some(run_id) => self.get_run(run_id).await.ok(),
            None => None,
        };
        let child_membership_completed_at_ms =
            flow_child_membership_completed_at_ms(&record, root_run.as_ref());
        let projection = self
            .flow_scope_projection(
                &record,
                root_run.as_ref(),
                child_membership_completed_at_ms,
                &mut projection_context,
            )
            .await?;
        let flow_status = derive_flow_status_from_scope(
            &record,
            root_run.as_ref(),
            &projection.runs,
            &projection.tasks,
        );
        let root_record = match record.run_id.as_deref() {
            Some(run_id) => self.run_record(run_id).await.ok(),
            None => None,
        };
        let root_generation = root_record
            .as_ref()
            .and_then(|record| submit_input_generation_from_payload(&record.payload));
        let root_provider = root_record
            .as_ref()
            .and_then(|record| submit_input_provider_from_payload(&record.payload))
            .or_else(|| {
                root_run
                    .as_ref()
                    .and_then(|run| run.request.provider.clone())
            });
        let root_model = root_generation
            .as_ref()
            .and_then(|generation| generation.model.clone())
            .or_else(|| root_run.as_ref().and_then(|run| run.request.model.clone()));
        let reasoning_high = root_generation
            .as_ref()
            .and_then(|generation| generation.reasoning.as_ref())
            .and_then(|reasoning| reasoning.effort)
            == Some(kheish_types::ReasoningEffort::High);

        let mut checks = Vec::new();
        checks.push(flow_check(
            "flow_status",
            flow_status == FlowStatus::Succeeded,
            format!("status={flow_status:?}"),
        ));
        checks.push(flow_check(
            "root_route",
            root_provider.as_deref() == Some("openai")
                && root_model.as_deref() == Some("gpt-5.4")
                && reasoning_high,
            format!(
                "provider={:?} model={:?} reasoning_high={}",
                root_provider, root_model, reasoning_high
            ),
        ));

        let root_run_id = record.run_id.as_deref();
        let sidechain_runs = projection
            .runs
            .iter()
            .filter(|run| Some(run.run_id.as_str()) != root_run_id)
            .collect::<Vec<_>>();
        let sidechain_reviewer_agent_ids = sidechain_runs
            .iter()
            .map(|run| run.agent_id.as_str())
            .collect::<BTreeSet<_>>();
        checks.push(flow_check(
            "reviewer_count",
            sidechain_reviewer_agent_ids.len() >= 3,
            format!(
                "sidechain_reviewer_count={} sidechain_run_count={}",
                sidechain_reviewer_agent_ids.len(),
                sidechain_runs.len()
            ),
        ));
        checks.push(flow_check(
            "sidechains_terminal_success",
            !sidechain_runs.is_empty()
                && sidechain_runs
                    .iter()
                    .all(|run| run.status == DaemonRunStatus::Completed),
            format!(
                "sidechain_statuses={}",
                sidechain_runs
                    .iter()
                    .map(|run| format!("{}:{:?}", run.run_id, run.status))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ));

        let bash_marker_found = self
            .flow_tasks_contain_marker(&projection, "PRODUCT_VIEW_PREP_DIR_READY")
            .await?;
        checks.push(flow_check(
            "bash_marker",
            bash_marker_found,
            "required marker PRODUCT_VIEW_PREP_DIR_READY".to_string(),
        ));

        let report_path = resolve_workspace_report_path(
            &self.system_prompt.environment().workspace_root,
            &request.report_path,
        )?;
        let report_relative_path = workspace_relative_report_path(&request.report_path)?;
        let report_content = std::fs::read_to_string(&report_path).ok();
        checks.push(flow_check(
            "report_path",
            report_content.is_some(),
            report_path.display().to_string(),
        ));
        let report_written_by_flow = self
            .flow_modified_report_path(
                &projection,
                root_run.as_ref(),
                &report_relative_path,
                record.created_at_ms,
            )
            .await?;
        checks.push(flow_check(
            "report_written_by_flow",
            report_written_by_flow,
            format!("path={report_relative_path}"),
        ));
        let required_sections = if request.required_sections.is_empty() {
            default_product_view_sections()
        } else {
            request.required_sections.clone()
        };
        let missing_sections = required_sections
            .iter()
            .filter(|section| {
                !report_content
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains(&section.to_ascii_lowercase())
            })
            .cloned()
            .collect::<Vec<_>>();
        checks.push(flow_check(
            "required_sections",
            missing_sections.is_empty(),
            if missing_sections.is_empty() {
                "all required sections present".to_string()
            } else {
                format!("missing={}", missing_sections.join(","))
            },
        ));

        let forbidden_tools = request.forbidden_tools.clone();
        let used_forbidden_tools = self
            .used_forbidden_tools(
                &projection,
                root_run.as_ref(),
                &forbidden_tools,
                record.created_at_ms,
            )
            .await?;
        checks.push(flow_check(
            "forbidden_tools",
            used_forbidden_tools.is_empty(),
            if used_forbidden_tools.is_empty() {
                "no forbidden tool calls found in scoped session journals".to_string()
            } else {
                format!("used={}", used_forbidden_tools.join(","))
            },
        ));

        let passed = checks.iter().all(|check| check.passed);
        Ok(ProductViewFlowVerificationVerdict {
            flow_id: flow_id.to_string(),
            passed,
            checks,
            evidence: json!({
                "run_ids": projection.primitive_refs.run_ids,
                "task_ids": projection.primitive_refs.task_ids,
                "agent_ids": projection.primitive_refs.agent_ids,
                "report_path": request.report_path,
            }),
        })
    }

    pub(crate) async fn append_flow_evidence(
        self: &Arc<Self>,
        flow_id: &str,
        request: AppendFlowEvidenceRequest,
    ) -> Result<FlowView> {
        let record = self.playbook_service.get_flow_record(flow_id).await?;
        let mut projection_context = FlowProjectionContext::default();
        let root_run = match record.run_id.as_deref() {
            Some(run_id) => self.get_run(run_id).await.ok(),
            None => None,
        };
        let child_membership_completed_at_ms =
            flow_child_membership_completed_at_ms(&record, root_run.as_ref());
        let projection = self
            .flow_scope_projection(
                &record,
                root_run.as_ref(),
                child_membership_completed_at_ms,
                &mut projection_context,
            )
            .await?;
        self.validate_flow_evidence_refs(
            &record,
            &projection.primitive_refs,
            &request.evidence_refs,
        )?;
        let record = self
            .playbook_service
            .append_flow_evidence(flow_id, request)
            .await?;
        self.flow_view_with_context(record, &mut FlowProjectionContext::default())
            .await
    }

    pub(crate) async fn cancel_flow(self: &Arc<Self>, flow_id: &str) -> Result<FlowView> {
        let mut projection_context = FlowProjectionContext::default();
        let mut record = self.playbook_service.get_flow_record(flow_id).await?;
        if record.run_id.is_none()
            && let Some(recovered_run_id) = self.find_flow_run_id(&record).await?
        {
            record = self
                .playbook_service
                .attach_flow_run(&record.flow_id, &recovered_run_id)
                .await?;
        }
        if record.run_id.is_none() {
            let record = self.playbook_service.cancel_pending_flow(flow_id).await?;
            return self
                .flow_view_with_context(record, &mut projection_context)
                .await;
        }

        let root_run = match record.run_id.as_deref() {
            Some(run_id) => self.get_run(run_id).await.ok(),
            None => None,
        };
        let child_membership_completed_at_ms =
            flow_child_membership_completed_at_ms(&record, root_run.as_ref());
        let projection = self
            .flow_scope_projection(
                &record,
                root_run.as_ref(),
                child_membership_completed_at_ms,
                &mut projection_context,
            )
            .await?;
        for run in projection
            .runs
            .iter()
            .filter(|run| !run.status.is_terminal())
        {
            let _ = self.cancel_run(&run.run_id).await?;
        }
        for task in projection.tasks.iter().filter(|task| {
            matches!(
                task.status,
                kheish_types::TaskStatus::Pending
                    | kheish_types::TaskStatus::InProgress
                    | kheish_types::TaskStatus::Blocked
            )
        }) {
            if let Some(session_id) = projection.task_session_ids.get(&task.id) {
                let _ = self
                    .stop_session_task(
                        session_id,
                        &task.id,
                        Some(format!("flow {flow_id} cancelled")),
                        None,
                    )
                    .await?;
            }
        }
        let record = self.playbook_service.get_flow_record(flow_id).await?;
        let mut projection_context = FlowProjectionContext::default();
        self.flow_view_with_context(record, &mut projection_context)
            .await
    }

    async fn flow_view(self: &Arc<Self>, record: FlowRecord) -> Result<FlowView> {
        let mut projection_context = FlowProjectionContext::default();
        self.flow_view_with_context(record, &mut projection_context)
            .await
    }

    async fn flow_view_with_context(
        self: &Arc<Self>,
        mut record: FlowRecord,
        projection_context: &mut FlowProjectionContext,
    ) -> Result<FlowView> {
        if record.run_id.is_none()
            && let Some(recovered_run_id) = self.find_flow_run_id(&record).await?
        {
            record = self
                .playbook_service
                .attach_flow_run(&record.flow_id, &recovered_run_id)
                .await?;
        }
        let run = match record.run_id.as_deref() {
            Some(run_id) => self.get_run(run_id).await.ok(),
            None => None,
        };
        let child_membership_completed_at_ms =
            flow_child_membership_completed_at_ms(&record, run.as_ref());
        let projection = self
            .flow_scope_projection(
                &record,
                run.as_ref(),
                child_membership_completed_at_ms,
                projection_context,
            )
            .await?;
        let base_status = derive_flow_status_from_scope(
            &record,
            run.as_ref(),
            &projection.runs,
            &projection.tasks,
        );
        let manifest = self
            .playbook_service
            .manifest_for_ref(&record.playbook_ref)
            .await?;
        let contract_completed_at_ms = flow_contract_completed_at_ms(
            &record,
            run.as_ref(),
            &projection,
            child_membership_completed_at_ms,
        );
        let contract = self
            .flow_contract_validation(
                &record,
                &manifest,
                &projection,
                run.as_ref(),
                contract_completed_at_ms,
            )
            .await?;
        let phase_states =
            flow_phase_states(&record, &manifest, &projection.primitive_refs, &base_status);
        let status = derive_flow_status_with_contract(base_status, &contract);
        if flow_status_is_terminal(&status) && record.completed_at_ms.is_none() {
            let completed_at_ms = flow_terminal_boundary_ms(&record, run.as_ref(), &projection);
            record = self
                .playbook_service
                .mark_flow_completed_at(&record.flow_id, completed_at_ms)
                .await?;
        }
        Ok(build_flow_view_from_scope(
            record,
            run,
            projection.primitive_refs,
            phase_states,
            contract,
            status,
        ))
    }

    async fn validate_flow_start_contract(
        &self,
        session_id: &str,
        manifest: &PlaybookManifest,
    ) -> Result<()> {
        if let Some(required) = manifest.scopes.required_capability_scope.as_ref() {
            let current = self
                .load_session_capability_scope(session_id)
                .await?
                .normalized();
            let restricted = current.restrict_with(&required.normalized());
            if restricted != current {
                anyhow::bail!("flow requires narrower session capability_scope");
            }
        }
        if let Some(required) = manifest.scopes.required_credential_scope.as_ref() {
            let current = self
                .load_session_credential_scope(session_id)
                .await?
                .normalized();
            let restricted = current.restrict_with(&required.normalized());
            if restricted != current {
                anyhow::bail!("flow requires narrower session credential_scope");
            }
        }
        Ok(())
    }

    async fn apply_flow_runtime_defaults(&self, request: &mut StartFlowRequest) -> Result<()> {
        let defaults = self
            .playbook_service
            .runtime_defaults(&request.playbook_ref)
            .await?;
        let request_provider = request.request.provider.clone();
        let default_provider = defaults.provider.clone();
        if request.request.provider.is_none() {
            request.request.provider = default_provider.clone();
        }
        if request
            .request
            .generation
            .as_ref()
            .and_then(|generation| generation.model.as_ref())
            .is_none()
            && let Some(model) = defaults.model
        {
            let provider_matches_default = request_provider
                .as_deref()
                .zip(default_provider.as_deref())
                .is_none_or(|(request_provider, default_provider)| {
                    request_provider == default_provider
                });
            if provider_matches_default {
                let generation = request
                    .request
                    .generation
                    .get_or_insert_with(kheish_types::ModelGenerationConfig::default);
                generation.model = Some(model);
            }
        }
        Ok(())
    }

    async fn flow_tasks_contain_marker(
        &self,
        projection: &FlowScopeProjection,
        marker: &str,
    ) -> Result<bool> {
        for task in &projection.tasks {
            let Some(session_id) = projection.task_session_ids.get(&task.id) else {
                continue;
            };
            let output = self
                .task_output_view(
                    session_id,
                    &task.id,
                    false,
                    std::time::Duration::from_millis(0),
                    128 * 1024,
                    true,
                )
                .await?;
            let text = output
                .output_text
                .or(output.output_excerpt)
                .unwrap_or_default();
            if text.contains(marker) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn used_forbidden_tools(
        &self,
        projection: &FlowScopeProjection,
        root_run: Option<&RunView>,
        forbidden_tools: &[String],
        flow_created_at_ms: u64,
    ) -> Result<Vec<String>> {
        if forbidden_tools.is_empty() {
            return Ok(Vec::new());
        }
        let forbidden = forbidden_tools
            .iter()
            .map(|tool| tool.as_str())
            .collect::<BTreeSet<_>>();
        let mut session_ids = projection
            .runs
            .iter()
            .map(|run| run.session_id.clone())
            .collect::<BTreeSet<_>>();
        if let Some(root_run) = root_run {
            session_ids.insert(root_run.session_id.clone());
        }
        session_ids.extend(projection.task_session_ids.values().cloned());
        let mut used = BTreeSet::new();
        for session_id in session_ids {
            let session = self.session_service.load_session(&session_id).await?;
            for entry in session.journal {
                if entry.timestamp_ms < flow_created_at_ms {
                    continue;
                }
                if let kheish_types::SessionEvent::ToolCallStarted { call } = entry.event
                    && forbidden.contains(call.name.as_str())
                {
                    used.insert(call.name);
                }
            }
        }
        Ok(used.into_iter().collect())
    }

    async fn flow_modified_report_path(
        &self,
        projection: &FlowScopeProjection,
        root_run: Option<&RunView>,
        report_relative_path: &str,
        flow_created_at_ms: u64,
    ) -> Result<bool> {
        let mut session_ids = projection
            .runs
            .iter()
            .map(|run| run.session_id.clone())
            .collect::<BTreeSet<_>>();
        if let Some(root_run) = root_run {
            session_ids.insert(root_run.session_id.clone());
        }
        session_ids.extend(projection.task_session_ids.values().cloned());
        for session_id in session_ids {
            let session = self.session_service.load_session(&session_id).await?;
            for entry in session.journal {
                if entry.timestamp_ms < flow_created_at_ms {
                    continue;
                }
                if let kheish_types::SessionEvent::ToolCallFinished { result } = entry.event {
                    if result.is_error {
                        continue;
                    }
                    if result.context_updates.iter().any(|update| {
                        matches!(
                            update,
                            kheish_types::ContextUpdate::FileModified { path }
                                if path == report_relative_path
                        )
                    }) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    async fn flow_contract_validation(
        &self,
        record: &FlowRecord,
        manifest: &PlaybookManifest,
        projection: &FlowScopeProjection,
        root_run: Option<&RunView>,
        flow_completed_at_ms: Option<u64>,
    ) -> Result<FlowContractValidation> {
        let mut checks = Vec::new();
        let top_level_missing = missing_evidence_requirements(
            &manifest.required_evidence,
            &record.evidence_refs,
            &projection.primitive_refs,
            &record.flow_id,
        );
        checks.push(flow_contract_check(
            "required_evidence",
            top_level_missing.is_empty(),
            if top_level_missing.is_empty() {
                "all top-level evidence requirements satisfied".to_string()
            } else {
                format!(
                    "missing={}",
                    format_evidence_requirements(&top_level_missing)
                )
            },
        ));

        for phase in &manifest.phases {
            let missing = missing_evidence_requirements(
                &phase.required_evidence,
                &record.evidence_refs,
                &projection.primitive_refs,
                &record.flow_id,
            );
            checks.push(flow_contract_check(
                format!("phase:{}:evidence", phase.phase_id),
                missing.is_empty(),
                if missing.is_empty() {
                    "phase evidence requirements satisfied".to_string()
                } else {
                    format!("missing={}", format_evidence_requirements(&missing))
                },
            ));
        }

        if manifest.tools.enforce {
            let violations = self
                .flow_tool_policy_violations(
                    record,
                    manifest,
                    projection,
                    root_run,
                    flow_completed_at_ms,
                )
                .await?;
            checks.push(flow_contract_check(
                "tool_policy",
                violations.is_empty(),
                if violations.is_empty() {
                    "no scoped tool-policy violations".to_string()
                } else {
                    format!("violations={}", violations.join(","))
                },
            ));
        }

        Ok(FlowContractValidation {
            passed: checks.iter().all(|check| check.passed),
            checks,
        })
    }

    async fn flow_tool_policy_violations(
        &self,
        record: &FlowRecord,
        manifest: &PlaybookManifest,
        projection: &FlowScopeProjection,
        root_run: Option<&RunView>,
        flow_completed_at_ms: Option<u64>,
    ) -> Result<Vec<String>> {
        let policy = &manifest.tools;
        let allow = policy
            .allow
            .iter()
            .map(|tool| tool.as_str())
            .collect::<BTreeSet<_>>();
        let deny = policy
            .deny
            .iter()
            .map(|tool| tool.as_str())
            .collect::<BTreeSet<_>>();
        let mut session_ids = projection
            .runs
            .iter()
            .map(|run| run.session_id.clone())
            .collect::<BTreeSet<_>>();
        if let Some(root_run) = root_run {
            session_ids.insert(root_run.session_id.clone());
        }
        session_ids.extend(projection.task_session_ids.values().cloned());
        let mut violations = BTreeSet::new();
        for session_id in session_ids {
            let session = self.session_service.load_session(&session_id).await?;
            for entry in session.journal {
                if entry.timestamp_ms < record.created_at_ms {
                    continue;
                }
                if let Some(completed_at_ms) = flow_completed_at_ms
                    && entry.timestamp_ms > completed_at_ms
                {
                    continue;
                }
                if let kheish_types::SessionEvent::ToolCallStarted { call } = entry.event {
                    let blocked = deny.contains(call.name.as_str())
                        || (!allow.is_empty() && !allow.contains(call.name.as_str()));
                    if blocked {
                        violations.insert(format!("{session_id}:{}", call.name));
                    }
                }
            }
        }
        Ok(violations.into_iter().collect())
    }

    fn validate_flow_evidence_refs(
        &self,
        record: &FlowRecord,
        primitive_refs: &FlowPrimitiveRefs,
        evidence_refs: &[crate::FlowEvidenceRef],
    ) -> Result<()> {
        for evidence in evidence_refs {
            if evidence.kind.trim().is_empty() {
                anyhow::bail!("evidence kind is required");
            }
            if evidence.id.trim().is_empty() {
                anyhow::bail!("evidence id is required");
            }
            if !flow_evidence_ref_resolves(record, primitive_refs, &evidence.kind, &evidence.id) {
                anyhow::bail!(
                    "evidence ref {}:{} does not resolve inside flow {}",
                    evidence.kind,
                    evidence.id,
                    record.flow_id
                );
            }
        }
        Ok(())
    }

    async fn find_flow_run_id(&self, record: &FlowRecord) -> Result<Option<String>> {
        for run in self.list_runs(Some(&record.session_id)).await? {
            if run_matches_flow_record(&run, record) {
                return Ok(Some(run.run_id));
            }
        }
        Ok(None)
    }

    async fn flow_scope_projection(
        self: &Arc<Self>,
        record: &FlowRecord,
        root_run: Option<&RunView>,
        flow_completed_at_ms: Option<u64>,
        context: &mut FlowProjectionContext,
    ) -> Result<FlowScopeProjection> {
        let mut run_ids = BTreeSet::new();
        let mut task_ids = BTreeSet::new();
        let mut agent_ids = BTreeSet::new();
        let mut descendant_agent_ids = BTreeSet::new();
        let mut approval_ids = BTreeSet::new();
        let mut question_ids = BTreeSet::new();
        let mut session_ids = BTreeSet::new();
        let mut child_session_ids = BTreeSet::new();
        let mut runs = BTreeMap::new();
        let mut tasks = BTreeMap::new();
        let mut task_session_ids = BTreeMap::new();
        let root_agent_id = root_run.map(|run| run.agent_id.clone());

        if let Some(run) = root_run {
            insert_scoped_run(
                run.clone(),
                &mut run_ids,
                &mut agent_ids,
                &mut approval_ids,
                &mut question_ids,
                &mut runs,
            );
            session_ids.insert(record.session_id.clone());
        } else if let Some(run_id) = record.run_id.as_ref() {
            run_ids.insert(run_id.clone());
        }

        let agent_snapshots = context.agent_snapshots(self).await?;
        let mut changed = true;
        while changed {
            changed = false;

            for snapshot in &agent_snapshots {
                let agent_id = snapshot.agent.id.0.clone();
                let is_root_agent = root_agent_id.as_deref() == Some(agent_id.as_str());
                let spawned_by_scoped_run = snapshot
                    .agent
                    .spawned_by_run_id
                    .as_ref()
                    .is_some_and(|run_id| run_ids.contains(run_id));
                let nested_under_scoped_child = snapshot
                    .agent
                    .parent
                    .as_ref()
                    .is_some_and(|parent| descendant_agent_ids.contains(&parent.0));
                let already_scoped = agent_ids.contains(&agent_id);

                if !(is_root_agent
                    || spawned_by_scoped_run
                    || nested_under_scoped_child
                    || already_scoped)
                {
                    continue;
                }

                if agent_ids.insert(agent_id.clone()) {
                    changed = true;
                }
                if !is_root_agent
                    && (spawned_by_scoped_run
                        || nested_under_scoped_child
                        || descendant_agent_ids.contains(&agent_id))
                    && descendant_agent_ids.insert(agent_id.clone())
                {
                    changed = true;
                }

                let session_id = snapshot
                    .agent
                    .sidechain_session_id
                    .as_ref()
                    .unwrap_or(&snapshot.agent.conversation.session_id)
                    .clone();
                if session_ids.insert(session_id.clone()) {
                    changed = true;
                }
                if !is_root_agent && child_session_ids.insert(session_id) {
                    changed = true;
                }

                for approval in &snapshot.pending_approvals {
                    approval_ids.insert(approval.id.clone());
                }
                for question in &snapshot.pending_questions {
                    question_ids.insert(question.id.clone());
                }
            }

            for session_id in session_ids.clone() {
                for run in context.runs_for_session(self, &session_id).await? {
                    let is_known_scoped_run = run_ids.contains(&run.run_id);
                    let belongs_to_current_flow_child = run_belongs_to_current_flow_child(
                        &run,
                        &session_id,
                        &child_session_ids,
                        &agent_ids,
                        record.created_at_ms,
                        flow_completed_at_ms,
                    );
                    if !(is_known_scoped_run || belongs_to_current_flow_child) {
                        continue;
                    }
                    let previous_run_count = run_ids.len();
                    insert_scoped_run(
                        run,
                        &mut run_ids,
                        &mut agent_ids,
                        &mut approval_ids,
                        &mut question_ids,
                        &mut runs,
                    );
                    if run_ids.len() != previous_run_count {
                        changed = true;
                    }
                }
            }
        }

        for run_id in run_ids.clone() {
            if let Ok(events) = context.events_for_run(self, &run_id) {
                for entry in events {
                    match entry.event {
                        RunEvent::WaitingForApproval { request_ids, .. } => {
                            approval_ids.extend(request_ids);
                        }
                        RunEvent::ApprovalResolved { resolutions } => {
                            approval_ids.extend(
                                resolutions
                                    .into_iter()
                                    .map(|resolution| resolution.request_id),
                            );
                        }
                        RunEvent::WaitingForUserQuestion { request_ids, .. } => {
                            question_ids.extend(request_ids);
                        }
                        _ => {}
                    }
                }
            }
        }

        for session_id in &session_ids {
            for task in context.tasks_for_session(self, session_id).await? {
                let created_by_run_id = task_created_by_run_id(&task).map(str::to_string);
                let created_by_scoped_run = created_by_run_id
                    .as_ref()
                    .is_some_and(|run_id| run_ids.contains(run_id));
                let owned_by_scoped_agent = task
                    .owner_agent_id
                    .as_ref()
                    .is_some_and(|owner| agent_ids.contains(owner));
                let belongs_to_current_flow_child = child_session_ids.contains(session_id)
                    && owned_by_scoped_agent
                    && timestamp_within_flow_window(
                        task.created_at_ms,
                        record.created_at_ms,
                        flow_completed_at_ms,
                    );
                let legacy_task_without_run_id = created_by_run_id.is_none();
                if !(created_by_scoped_run
                    || (legacy_task_without_run_id && belongs_to_current_flow_child))
                {
                    continue;
                }
                task_ids.insert(task.id.clone());
                task_session_ids.insert(task.id.clone(), session_id.clone());
                tasks.insert(task.id.clone(), task);
            }
        }

        Ok(FlowScopeProjection {
            primitive_refs: FlowPrimitiveRefs {
                run_ids: run_ids.into_iter().collect(),
                task_ids: task_ids.into_iter().collect(),
                agent_ids: agent_ids.into_iter().collect(),
                approval_ids: approval_ids.into_iter().collect(),
                question_ids: question_ids.into_iter().collect(),
                schedule_ids: Vec::new(),
                output_ids: Vec::new(),
            },
            runs: runs.into_values().collect(),
            tasks: tasks.into_values().collect(),
            task_session_ids,
        })
    }
}

fn metadata_can_accept_daemon_key(metadata: &Option<serde_json::Value>) -> bool {
    matches!(
        metadata,
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::Object(_))
    )
}

#[derive(Debug, Default)]
struct FlowScopeProjection {
    primitive_refs: FlowPrimitiveRefs,
    runs: Vec<RunView>,
    tasks: Vec<kheish_types::TaskRecord>,
    task_session_ids: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct FlowProjectionContext {
    agent_snapshots: Option<Vec<kheish_agent::ManagedAgentSnapshot>>,
    runs_by_session: BTreeMap<String, Vec<RunView>>,
    tasks_by_session: BTreeMap<String, Vec<kheish_types::TaskRecord>>,
    events_by_run: BTreeMap<String, Vec<RunEventEntry>>,
}

impl FlowProjectionContext {
    async fn agent_snapshots<M>(
        &mut self,
        state: &Arc<DaemonState<M>>,
    ) -> Result<Vec<kheish_agent::ManagedAgentSnapshot>>
    where
        M: kheish_core::ModelDriver + Send + Sync + 'static,
    {
        if self.agent_snapshots.is_none() {
            self.agent_snapshots = Some(state.list_agents().await?);
        }
        Ok(self.agent_snapshots.clone().unwrap_or_default())
    }

    async fn runs_for_session<M>(
        &mut self,
        state: &Arc<DaemonState<M>>,
        session_id: &str,
    ) -> Result<Vec<RunView>>
    where
        M: kheish_core::ModelDriver + Send + Sync + 'static,
    {
        if !self.runs_by_session.contains_key(session_id) {
            self.runs_by_session.insert(
                session_id.to_string(),
                state.list_runs(Some(session_id)).await?,
            );
        }
        Ok(self
            .runs_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn tasks_for_session<M>(
        &mut self,
        state: &Arc<DaemonState<M>>,
        session_id: &str,
    ) -> Result<Vec<kheish_types::TaskRecord>>
    where
        M: kheish_core::ModelDriver + Send + Sync + 'static,
    {
        if !self.tasks_by_session.contains_key(session_id) {
            self.tasks_by_session.insert(
                session_id.to_string(),
                state.load_session_control_state(session_id).await?.tasks,
            );
        }
        Ok(self
            .tasks_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    fn events_for_run<M>(
        &mut self,
        state: &DaemonState<M>,
        run_id: &str,
    ) -> Result<Vec<RunEventEntry>>
    where
        M: kheish_core::ModelDriver + Send + Sync + 'static,
    {
        if !self.events_by_run.contains_key(run_id) {
            self.events_by_run
                .insert(run_id.to_string(), state.run_events(run_id)?);
        }
        Ok(self.events_by_run.get(run_id).cloned().unwrap_or_default())
    }
}

fn submit_input_generation_from_payload(
    payload: &RunRequestPayload,
) -> Option<kheish_types::ModelGenerationConfig> {
    match payload {
        RunRequestPayload::Input { request, .. }
        | RunRequestPayload::ScheduledInput { request, .. } => request.generation.clone(),
        RunRequestPayload::ApprovalResume {
            original_request, ..
        }
        | RunRequestPayload::UserQuestionResume {
            original_request, ..
        } => original_request
            .as_ref()
            .and_then(|request| request.generation.clone()),
        RunRequestPayload::ObservationMaterialization { .. }
        | RunRequestPayload::ScheduledObservationMaterialization { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
    }
}

fn submit_input_provider_from_payload(payload: &RunRequestPayload) -> Option<String> {
    match payload {
        RunRequestPayload::Input { request, .. }
        | RunRequestPayload::ScheduledInput { request, .. } => request.provider.clone(),
        RunRequestPayload::ApprovalResume {
            original_request, ..
        }
        | RunRequestPayload::UserQuestionResume {
            original_request, ..
        } => original_request
            .as_ref()
            .and_then(|request| request.provider.clone()),
        RunRequestPayload::ObservationMaterialization { .. }
        | RunRequestPayload::ScheduledObservationMaterialization { .. }
        | RunRequestPayload::ChannelDelivery { .. }
        | RunRequestPayload::MailboxDelivery { .. }
        | RunRequestPayload::ParentClarification { .. } => None,
    }
}

fn flow_check(
    name: impl Into<String>,
    passed: bool,
    details: impl Into<String>,
) -> FlowVerificationCheck {
    FlowVerificationCheck {
        name: name.into(),
        passed,
        details: details.into(),
    }
}

fn default_product_view_sections() -> Vec<String> {
    [
        "Executive Summary",
        "Product View",
        "Customer Context",
        "Risks",
        "Recommendations",
        "Evidence",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn resolve_workspace_report_path(
    workspace_root: &Path,
    report_path: &str,
) -> Result<std::path::PathBuf> {
    Ok(workspace_root.join(workspace_relative_report_path(report_path)?))
}

fn workspace_relative_report_path(report_path: &str) -> Result<String> {
    let candidate = std::path::Path::new(report_path);
    anyhow::ensure!(
        !candidate.is_absolute(),
        "report_path must be workspace-relative"
    );
    anyhow::ensure!(
        candidate.components().all(|component| !matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )),
        "report_path must stay inside the workspace"
    );
    let components = candidate
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!components.is_empty(), "report_path must reference a file");
    Ok(components.join("/"))
}

fn insert_scoped_run(
    run: RunView,
    run_ids: &mut BTreeSet<String>,
    agent_ids: &mut BTreeSet<String>,
    approval_ids: &mut BTreeSet<String>,
    question_ids: &mut BTreeSet<String>,
    runs: &mut BTreeMap<String, RunView>,
) {
    run_ids.insert(run.run_id.clone());
    agent_ids.insert(run.agent_id.clone());
    approval_ids.extend(run.pending_approval_ids.clone());
    question_ids.extend(run.pending_question_ids.clone());
    runs.insert(run.run_id.clone(), run);
}

fn task_created_by_run_id(task: &kheish_types::TaskRecord) -> Option<&str> {
    task.metadata
        .get("created_by_run_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            task.metadata
                .get("kheish_daemon")
                .and_then(|metadata| metadata.get("created_by_run_id"))
                .and_then(serde_json::Value::as_str)
        })
}

fn run_belongs_to_current_flow_child(
    run: &RunView,
    session_id: &str,
    child_session_ids: &BTreeSet<String>,
    agent_ids: &BTreeSet<String>,
    flow_created_at_ms: u64,
    flow_completed_at_ms: Option<u64>,
) -> bool {
    child_session_ids.contains(session_id)
        && agent_ids.contains(&run.agent_id)
        && timestamp_within_flow_window(
            run.submitted_at_ms,
            flow_created_at_ms,
            flow_completed_at_ms,
        )
}

fn timestamp_within_flow_window(
    timestamp_ms: u64,
    created_at_ms: u64,
    completed_at_ms: Option<u64>,
) -> bool {
    timestamp_ms >= created_at_ms && completed_at_ms.is_none_or(|cutoff| timestamp_ms <= cutoff)
}

fn build_flow_view_from_scope(
    record: FlowRecord,
    run: Option<RunView>,
    primitive_refs: FlowPrimitiveRefs,
    phase_states: Vec<FlowPhaseState>,
    contract: FlowContractValidation,
    status: FlowStatus,
) -> FlowView {
    let run_stream_url = record
        .run_id
        .as_ref()
        .map(|run_id| format!("/v1/runs/{run_id}/stream"));
    FlowView {
        flow_id: record.flow_id,
        status,
        playbook_ref: record.playbook_ref,
        session_id: record.session_id,
        run_id: record.run_id,
        primitive_refs,
        phase_states,
        contract,
        run,
        run_stream_url,
        created_at_ms: record.created_at_ms,
        updated_at_ms: record.updated_at_ms,
        completed_at_ms: record.completed_at_ms,
        metadata: record.metadata,
        evidence_refs: record.evidence_refs,
    }
}

fn flow_status_is_terminal(status: &FlowStatus) -> bool {
    matches!(
        status,
        FlowStatus::Succeeded
            | FlowStatus::Failed
            | FlowStatus::Cancelled
            | FlowStatus::Interrupted
    )
}

fn derive_flow_status_with_contract(
    base_status: FlowStatus,
    contract: &FlowContractValidation,
) -> FlowStatus {
    if base_status == FlowStatus::Succeeded && !contract.passed {
        FlowStatus::Failed
    } else {
        base_status
    }
}

fn flow_terminal_boundary_ms(
    record: &FlowRecord,
    root_run: Option<&RunView>,
    projection: &FlowScopeProjection,
) -> u64 {
    let mut boundary = record.updated_at_ms.max(record.created_at_ms);
    if let Some(root_run) = root_run {
        boundary = boundary.max(root_run.updated_at_ms);
    }
    for run in &projection.runs {
        boundary = boundary.max(run.updated_at_ms);
    }
    for task in &projection.tasks {
        boundary = boundary.max(task.updated_at_ms);
    }
    boundary
}

fn flow_contract_check(
    name: impl Into<String>,
    passed: bool,
    details: impl Into<String>,
) -> FlowContractCheck {
    FlowContractCheck {
        name: name.into(),
        passed,
        details: details.into(),
    }
}

fn flow_phase_states(
    record: &FlowRecord,
    manifest: &PlaybookManifest,
    primitive_refs: &FlowPrimitiveRefs,
    base_status: &FlowStatus,
) -> Vec<FlowPhaseState> {
    manifest
        .phases
        .iter()
        .map(|phase| {
            let missing_evidence = missing_evidence_requirements(
                &phase.required_evidence,
                &record.evidence_refs,
                primitive_refs,
                &record.flow_id,
            );
            let status = if missing_evidence.is_empty() {
                FlowPhaseStatus::Satisfied
            } else if flow_status_is_terminal(base_status) {
                FlowPhaseStatus::Blocked
            } else {
                FlowPhaseStatus::Pending
            };
            FlowPhaseState {
                phase_id: phase.phase_id.clone(),
                status,
                missing_evidence,
            }
        })
        .collect()
}

fn missing_evidence_requirements(
    requirements: &[PlaybookEvidenceRequirement],
    evidence_refs: &[crate::FlowEvidenceRef],
    primitive_refs: &FlowPrimitiveRefs,
    flow_id: &str,
) -> Vec<PlaybookEvidenceRequirement> {
    requirements
        .iter()
        .filter(|requirement| {
            !evidence_refs
                .iter()
                .any(|evidence| evidence.kind == requirement.kind && evidence.id == requirement.id)
                && !flow_primitive_ref_contains(
                    primitive_refs,
                    flow_id,
                    &requirement.kind,
                    &requirement.id,
                )
        })
        .cloned()
        .collect()
}

fn flow_evidence_ref_resolves(
    record: &FlowRecord,
    primitive_refs: &FlowPrimitiveRefs,
    kind: &str,
    id: &str,
) -> bool {
    if is_known_flow_evidence_kind(kind) {
        flow_primitive_ref_contains(primitive_refs, &record.flow_id, kind, id)
    } else {
        true
    }
}

fn validate_start_flow_evidence_refs(evidence_refs: &[crate::FlowEvidenceRef]) -> Result<()> {
    for evidence in evidence_refs {
        if evidence.kind.trim().is_empty() {
            anyhow::bail!("evidence kind is required");
        }
        if evidence.id.trim().is_empty() {
            anyhow::bail!("evidence id is required");
        }
        if is_known_flow_evidence_kind(&evidence.kind) {
            anyhow::bail!(
                "known daemon evidence ref {}:{} must be appended after the flow projection exists",
                evidence.kind,
                evidence.id
            );
        }
    }
    Ok(())
}

fn is_known_flow_evidence_kind(kind: &str) -> bool {
    matches!(
        kind,
        "run" | "task" | "agent" | "approval" | "question" | "schedule" | "output" | "flow"
    )
}

fn flow_child_membership_completed_at_ms(
    record: &FlowRecord,
    root_run: Option<&RunView>,
) -> Option<u64> {
    record
        .completed_at_ms
        .or(record.cancelled_at_ms)
        .or_else(|| root_run.and_then(terminal_run_boundary_ms))
}

fn flow_contract_completed_at_ms(
    record: &FlowRecord,
    root_run: Option<&RunView>,
    projection: &FlowScopeProjection,
    child_membership_completed_at_ms: Option<u64>,
) -> Option<u64> {
    record.completed_at_ms.or_else(|| {
        child_membership_completed_at_ms
            .map(|_| flow_terminal_boundary_ms(record, root_run, projection))
    })
}

fn terminal_run_boundary_ms(run: &RunView) -> Option<u64> {
    run.status
        .is_terminal()
        .then(|| run.finished_at_ms.unwrap_or(run.updated_at_ms))
}

fn flow_primitive_ref_contains(
    primitive_refs: &FlowPrimitiveRefs,
    flow_id: &str,
    kind: &str,
    id: &str,
) -> bool {
    match kind {
        "flow" => id == flow_id,
        "run" => primitive_refs
            .run_ids
            .iter()
            .any(|candidate| candidate == id),
        "task" => primitive_refs
            .task_ids
            .iter()
            .any(|candidate| candidate == id),
        "agent" => primitive_refs
            .agent_ids
            .iter()
            .any(|candidate| candidate == id),
        "approval" => primitive_refs
            .approval_ids
            .iter()
            .any(|candidate| candidate == id),
        "question" => primitive_refs
            .question_ids
            .iter()
            .any(|candidate| candidate == id),
        "schedule" => primitive_refs
            .schedule_ids
            .iter()
            .any(|candidate| candidate == id),
        "output" => primitive_refs
            .output_ids
            .iter()
            .any(|candidate| candidate == id),
        _ => false,
    }
}

fn format_evidence_requirements(requirements: &[PlaybookEvidenceRequirement]) -> String {
    requirements
        .iter()
        .map(|requirement| format!("{}:{}", requirement.kind, requirement.id))
        .collect::<Vec<_>>()
        .join(",")
}

fn derive_flow_status_from_scope(
    record: &FlowRecord,
    root_run: Option<&RunView>,
    scoped_runs: &[RunView],
    scoped_tasks: &[kheish_types::TaskRecord],
) -> FlowStatus {
    if record.cancelled_at_ms.is_some() && root_run.is_none() {
        return FlowStatus::Cancelled;
    }
    if root_run.is_none() && scoped_runs.is_empty() {
        return if record.run_id.is_some() {
            FlowStatus::Unknown
        } else {
            FlowStatus::Pending
        };
    }
    if scoped_runs
        .iter()
        .any(|run| run.status == DaemonRunStatus::Failed)
        || scoped_tasks
            .iter()
            .any(|task| task.status == kheish_types::TaskStatus::Failed)
    {
        return FlowStatus::Failed;
    }
    if scoped_runs
        .iter()
        .any(|run| run.status == DaemonRunStatus::Cancelled)
        || scoped_tasks
            .iter()
            .any(|task| task.status == kheish_types::TaskStatus::Cancelled)
    {
        return FlowStatus::Cancelled;
    }
    if scoped_runs
        .iter()
        .any(|run| run.status == DaemonRunStatus::Interrupted)
    {
        return FlowStatus::Interrupted;
    }
    if scoped_runs.iter().any(|run| {
        matches!(
            run.status,
            DaemonRunStatus::WaitingForApproval | DaemonRunStatus::WaitingForUserQuestion
        )
    }) || scoped_tasks
        .iter()
        .any(|task| task.status == kheish_types::TaskStatus::Blocked)
    {
        return FlowStatus::Waiting;
    }
    if scoped_runs.iter().any(|run| {
        matches!(
            run.status,
            DaemonRunStatus::Queued | DaemonRunStatus::Running
        )
    }) || scoped_tasks.iter().any(|task| {
        matches!(
            task.status,
            kheish_types::TaskStatus::Pending | kheish_types::TaskStatus::InProgress
        )
    }) {
        return FlowStatus::Running;
    }
    let all_runs_succeeded = !scoped_runs.is_empty()
        && scoped_runs
            .iter()
            .all(|run| run.status == DaemonRunStatus::Completed);
    let all_tasks_succeeded = scoped_tasks
        .iter()
        .all(|task| task.status == kheish_types::TaskStatus::Completed);
    if all_runs_succeeded && all_tasks_succeeded {
        return FlowStatus::Succeeded;
    }

    let Some(run) = root_run else {
        return FlowStatus::Unknown;
    };
    match run.status {
        DaemonRunStatus::Queued | DaemonRunStatus::Running => FlowStatus::Running,
        DaemonRunStatus::WaitingForApproval | DaemonRunStatus::WaitingForUserQuestion => {
            FlowStatus::Waiting
        }
        DaemonRunStatus::Completed => FlowStatus::Succeeded,
        DaemonRunStatus::Failed => FlowStatus::Failed,
        DaemonRunStatus::Cancelled => FlowStatus::Cancelled,
        DaemonRunStatus::Interrupted => FlowStatus::Interrupted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_flow_record(root_run_id: Option<&str>) -> FlowRecord {
        FlowRecord {
            flow_id: "flow-test".to_string(),
            idempotency_key: None,
            playbook_ref: crate::PlaybookVersionRef {
                playbook_id: "playbook-test".to_string(),
                version: "1.0.0".to_string(),
                digest: "digest".to_string(),
            },
            session_id: "root-session".to_string(),
            input_digest: "input-digest".to_string(),
            correlation_nonce: "nonce".to_string(),
            run_id: root_run_id.map(str::to_string),
            created_at_ms: 1,
            updated_at_ms: 1,
            completed_at_ms: None,
            cancelled_at_ms: None,
            metadata: serde_json::Value::Null,
            evidence_refs: Vec::new(),
        }
    }

    fn test_run(
        run_id: &str,
        session_id: &str,
        agent_id: &str,
        status: DaemonRunStatus,
    ) -> RunView {
        test_run_at(run_id, session_id, agent_id, status, 1)
    }

    fn test_run_at(
        run_id: &str,
        session_id: &str,
        agent_id: &str,
        status: DaemonRunStatus,
        submitted_at_ms: u64,
    ) -> RunView {
        RunView {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            kind: DaemonRunKind::Input,
            status,
            submitted_at_ms,
            updated_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
            queued_position: None,
            request: crate::runs::RunRequestSummary {
                source_plugin: "test".to_string(),
                source_kind: "test".to_string(),
                actor_id: "test".to_string(),
                text_preview: None,
                provider: None,
                model: None,
                approval_count: None,
                question_count: None,
            },
            input_attachments: Vec::new(),
            input_metadata: None,
            pending_approval_ids: Vec::new(),
            pending_approvals: Vec::new(),
            pending_question_ids: Vec::new(),
            pending_questions: Vec::new(),
            outputs: Vec::new(),
            deliveries: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn flow_status_marks_interrupted_child_before_success() {
        let record = test_flow_record(Some("run-root"));
        let root = test_run(
            "run-root",
            "root-session",
            "agent-root",
            DaemonRunStatus::Completed,
        );
        let child = test_run(
            "run-child",
            "child-session",
            "agent-child",
            DaemonRunStatus::Interrupted,
        );

        assert_eq!(
            derive_flow_status_from_scope(
                &record,
                Some(&root),
                &[root.clone(), child],
                &Vec::new()
            ),
            FlowStatus::Interrupted
        );
    }

    #[test]
    fn flow_status_fails_closed_with_interrupted_root_and_completed_child() {
        let record = test_flow_record(Some("run-root"));
        let root = test_run(
            "run-root",
            "root-session",
            "agent-root",
            DaemonRunStatus::Interrupted,
        );
        let child = test_run(
            "run-child",
            "child-session",
            "agent-child",
            DaemonRunStatus::Completed,
        );

        assert_eq!(
            derive_flow_status_from_scope(
                &record,
                Some(&root),
                &[root.clone(), child],
                &Vec::new()
            ),
            FlowStatus::Interrupted
        );
    }

    #[test]
    fn flow_status_requires_all_scoped_runs_to_succeed() {
        let record = test_flow_record(Some("run-root"));
        let root = test_run(
            "run-root",
            "root-session",
            "agent-root",
            DaemonRunStatus::Completed,
        );
        let child = test_run(
            "run-child",
            "child-session",
            "agent-child",
            DaemonRunStatus::Running,
        );

        assert_eq!(
            derive_flow_status_from_scope(
                &record,
                Some(&root),
                &[root.clone(), child],
                &Vec::new()
            ),
            FlowStatus::Running
        );
    }

    #[test]
    fn flow_status_fails_closed_when_contract_fails() {
        assert_eq!(
            derive_flow_status_with_contract(
                FlowStatus::Succeeded,
                &FlowContractValidation {
                    passed: false,
                    checks: vec![FlowContractCheck {
                        name: "required_evidence".to_string(),
                        passed: false,
                        details: "missing=manual:review".to_string(),
                    }],
                },
            ),
            FlowStatus::Failed
        );
        assert_eq!(
            derive_flow_status_with_contract(
                FlowStatus::Running,
                &FlowContractValidation::default()
            ),
            FlowStatus::Running
        );
    }

    #[test]
    fn flow_phase_state_blocks_terminal_flow_when_evidence_is_missing() {
        let mut record = test_flow_record(Some("run-root"));
        record.flow_id = "flow-evidence".to_string();
        let manifest = PlaybookManifest {
            playbook_id: "playbook-test".to_string(),
            version: "1.0.0".to_string(),
            title: "Flow evidence".to_string(),
            objective: "Require evidence before success.".to_string(),
            description: None,
            inputs: Vec::new(),
            preconditions: Vec::new(),
            roles: Vec::new(),
            phases: vec![crate::PlaybookPhase {
                phase_id: "review".to_string(),
                objective: "Review evidence.".to_string(),
                acceptance_criteria: Vec::new(),
                required_evidence: vec![PlaybookEvidenceRequirement {
                    kind: "manual".to_string(),
                    id: "review".to_string(),
                    description: None,
                }],
            }],
            acceptance_criteria: Vec::new(),
            evidence_expectations: Vec::new(),
            required_evidence: Vec::new(),
            tools: Default::default(),
            runtime_defaults: Default::default(),
            scopes: Default::default(),
            metadata: serde_json::Value::Null,
        };
        let states = flow_phase_states(
            &record,
            &manifest,
            &FlowPrimitiveRefs::default(),
            &FlowStatus::Succeeded,
        );
        assert_eq!(states[0].status, FlowPhaseStatus::Blocked);
        assert_eq!(states[0].missing_evidence.len(), 1);

        record.evidence_refs.push(crate::FlowEvidenceRef {
            kind: "manual".to_string(),
            id: "review".to_string(),
            description: None,
        });
        let states = flow_phase_states(
            &record,
            &manifest,
            &FlowPrimitiveRefs::default(),
            &FlowStatus::Succeeded,
        );
        assert_eq!(states[0].status, FlowPhaseStatus::Satisfied);
        assert!(states[0].missing_evidence.is_empty());
    }

    #[test]
    fn flow_scope_excludes_old_runs_from_reused_child_sessions() {
        let child_session_ids = BTreeSet::from(["child-session".to_string()]);
        let agent_ids = BTreeSet::from(["agent-child".to_string()]);
        let old_failed_run = test_run_at(
            "run-old",
            "child-session",
            "agent-child",
            DaemonRunStatus::Failed,
            10,
        );
        let current_run = test_run_at(
            "run-current",
            "child-session",
            "agent-child",
            DaemonRunStatus::Running,
            100,
        );
        let foreign_agent_run = test_run_at(
            "run-foreign",
            "child-session",
            "agent-foreign",
            DaemonRunStatus::Failed,
            100,
        );

        assert!(!run_belongs_to_current_flow_child(
            &old_failed_run,
            "child-session",
            &child_session_ids,
            &agent_ids,
            50,
            None
        ));
        assert!(run_belongs_to_current_flow_child(
            &current_run,
            "child-session",
            &child_session_ids,
            &agent_ids,
            50,
            None
        ));
        assert!(!run_belongs_to_current_flow_child(
            &foreign_agent_run,
            "child-session",
            &child_session_ids,
            &agent_ids,
            50,
            None
        ));
        assert!(!run_belongs_to_current_flow_child(
            &current_run,
            "child-session",
            &child_session_ids,
            &agent_ids,
            50,
            Some(90)
        ));
    }

    #[test]
    fn flow_terminal_root_run_bounds_unsealed_child_session_reuse() {
        let record = test_flow_record(Some("run-root"));
        let mut root = test_run_at(
            "run-root",
            "root-session",
            "agent-root",
            DaemonRunStatus::Completed,
            50,
        );
        root.updated_at_ms = 90;
        root.finished_at_ms = Some(90);
        let cutoff = flow_child_membership_completed_at_ms(&record, Some(&root));
        assert_eq!(cutoff, Some(90));

        let child_session_ids = BTreeSet::from(["child-session".to_string()]);
        let agent_ids = BTreeSet::from(["agent-child".to_string()]);
        let late_reuse_run = test_run_at(
            "run-late-reuse",
            "child-session",
            "agent-child",
            DaemonRunStatus::Failed,
            100,
        );
        assert!(!run_belongs_to_current_flow_child(
            &late_reuse_run,
            "child-session",
            &child_session_ids,
            &agent_ids,
            record.created_at_ms,
            cutoff,
        ));
    }

    #[test]
    fn workspace_relative_report_path_normalizes_and_rejects_escapes() {
        assert_eq!(
            workspace_relative_report_path("./reports/product-view.md").unwrap(),
            "reports/product-view.md"
        );
        assert!(workspace_relative_report_path("../outside.md").is_err());
        assert!(workspace_relative_report_path("/tmp/outside.md").is_err());
        assert!(workspace_relative_report_path(".").is_err());
    }
}
