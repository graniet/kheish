#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use serde_json::json;

    use crate::{
        AgentId, AgentOrchestrator, AgentStatus, AgentSupervisor, AgentSupervisorAuditSink,
        ChildRetentionPolicy, ForkContext, MailboxMessage, ManagedAgentSnapshot, SubtaskSpec,
    };
    use kheish_core::LoopPolicy;
    use kheish_output::{OutputHost, OutputManifest, OutputPlugin, ResponseEnvelope};
    use kheish_runtime::{
        AgentRuntimeDependencies, InMemoryObserver, McpRuntimeSurface, ModelBudget,
        ModelRetryPolicy, ModelRuntime, ModelStreamEvent, PermissionBehavior, PermissionEngine,
        PermissionRule, PermissionScope, PromptMergeMode, ProviderError, SandboxProfile,
        SystemPromptBuilder, SystemPromptEnvironment, SystemPromptSettings, Tool, ToolContext,
        ToolDescriptor, ToolExecutionOutput, ToolInputKind, ToolRuntime, ToolSchema,
        ToolSchemaField,
    };
    use kheish_session::FileSessionStore;
    use kheish_types::{
        ApprovalResolution, ConversationKey, InputEnvelope, ModelGenerationConfig,
        ToolSurfaceFilter,
    };

    struct ScriptedProvider(Mutex<VecDeque<Result<Vec<ModelStreamEvent>, ProviderError>>>);

    #[async_trait]
    impl kheish_runtime::ModelProvider for ScriptedProvider {
        async fn stream(
            &self,
            _request: kheish_runtime::ModelRuntimeRequest,
            sink: kheish_runtime::ModelEventSink,
        ) -> std::result::Result<(), ProviderError> {
            match self.0.lock().pop_front().expect("scripted response") {
                Ok(events) => {
                    for event in events {
                        sink.emit(event).expect("sink should remain open");
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    }

    struct EchoTool;

    struct DiscardOutputPlugin;

    struct FailingAuditSink;

    impl AgentSupervisorAuditSink for FailingAuditSink {
        fn append_supervisor_audit(&self, _entry: &crate::AgentSupervisorAuditEntry) -> Result<()> {
            Err(anyhow::anyhow!("audit sink unavailable"))
        }
    }

    #[async_trait]
    impl OutputPlugin for DiscardOutputPlugin {
        fn manifest(&self) -> OutputManifest {
            OutputManifest {
                name: "memory".to_string(),
                version: "0.1.0".to_string(),
                description: "Discard outputs during agent tests.".to_string(),
            }
        }

        async fn deliver(&self, _response: ResponseEnvelope) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "echo".to_string(),
                description: "Echoes the provided text".to_string(),
                schema: ToolSchema {
                    fields: vec![ToolSchemaField {
                        name: "text".to_string(),
                        kind: ToolInputKind::String,
                        item_kind: None,
                        structured_schema: None,
                        required: true,
                        description: Some("Text to echo.".to_string()),
                    }],
                },
                timeout_ms: 100,
                sandbox: SandboxProfile::ReadOnly,
                allows_parallel: true,
            }
        }

        async fn execute(
            &self,
            _ctx: ToolContext,
            input: serde_json::Value,
        ) -> Result<ToolExecutionOutput> {
            Ok(ToolExecutionOutput::json(json!({"echo": input["text"]})))
        }
    }

    fn orchestrator_fixture(
        provider: ScriptedProvider,
    ) -> (
        AgentOrchestrator<ModelRuntime<ScriptedProvider>>,
        Arc<AgentSupervisor>,
    ) {
        let observer = InMemoryObserver::shared();
        let model = Arc::new(ModelRuntime::new(
            provider,
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer.clone(),
        ));
        let mut tools = ToolRuntime::new(observer.clone());
        tools.register(EchoTool);
        let tools = Arc::new(tools);
        let permissions = Arc::new(PermissionEngine::new(
            vec![],
            vec![],
            vec![PermissionRule {
                scope: PermissionScope::Session,
                tool_name_pattern: "echo".to_string(),
                behavior: PermissionBehavior::Ask,
                reason: Some("approval required".to_string()),
            }],
            observer.clone(),
        ));
        let sessions = Arc::new(FileSessionStore::new(
            std::env::temp_dir().join("kheish-agent-tests"),
        ));
        let mut outputs = OutputHost::new();
        outputs.register(DiscardOutputPlugin);
        let outputs = Arc::new(outputs);
        let supervisor = Arc::new(AgentSupervisor::new(observer.clone()));
        let orchestrator = AgentOrchestrator::new(
            LoopPolicy::default(),
            AgentRuntimeDependencies {
                model,
                tools,
                permissions,
                sessions,
                outputs,
                hooks: Arc::new(kheish_core::NoopHookDispatcher),
                system_prompt: Arc::new(SystemPromptBuilder::new(
                    SystemPromptEnvironment::new("/tmp", "/bin/bash"),
                    SystemPromptSettings::default(),
                )),
                observer,
                skills: Arc::default(),
                active_plugins: Vec::new(),
                active_mcp_tools: Vec::new(),
                connected_mcp_servers: Vec::new(),
                credentialed_mcp_servers: Vec::new(),
                mcp_tool_servers: BTreeMap::new(),
                mcp_server_instructions: Vec::new(),
                mcp_surface: Arc::new(parking_lot::RwLock::new(McpRuntimeSurface::default())),
                mcp_hydrator: None,
            },
            supervisor.clone(),
        );
        (orchestrator, supervisor)
    }

    #[tokio::test]
    async fn supervisor_spawns_agents_and_routes_mail() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer);
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.spawn(
            Some(parent.id.clone()),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: None,
            },
            Some("review"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        assert!(
            supervisor
                .spawn(
                    Some(parent.id.clone()),
                    ConversationKey {
                        session_id: "child".to_string(),
                        thread_id: Some("duplicate-thread".to_string()),
                    },
                    None,
                    None,
                    ChildRetentionPolicy::Retain,
                    None,
                    None,
                )
                .is_err(),
            "supervisor should reject duplicate live session ids"
        );

        supervisor.assign_subtask(
            &child.id,
            SubtaskSpec {
                name: "review".to_string(),
                description: "Review output".to_string(),
                input: InputEnvelope::text("memory", "test", "child", "user-1", "review"),
            },
        )?;
        supervisor.set_status(&child.id, AgentStatus::Running)?;
        supervisor.post(MailboxMessage::new(
            "mailbox-test-1".to_string(),
            parent.id.clone(),
            child.id.clone(),
            "work".to_string(),
            json!({"hello": true}),
            0,
            None,
        ));

        let status = supervisor.status_snapshot();
        assert_eq!(status.total, 2);
        assert_eq!(status.sidechain_count, 1);
        assert_eq!(status.running, 1);
        assert_eq!(status.idle, 1);
        assert_eq!(status.mailbox_message_count, 1);
        assert_eq!(supervisor.mailbox_counts().get(&child.id), Some(&1));
        assert!(supervisor.shares_root_with(&parent.id, &child.id)?);
        assert_eq!(
            supervisor
                .root_tree_records(&child.id)?
                .into_iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![parent.id.clone(), child.id.clone()]
        );

        let messages = supervisor.drain_mailbox(&child.id);
        assert_eq!(messages.len(), 1);
        assert!(supervisor.mailbox_counts().is_empty());
        assert_eq!(
            supervisor
                .get(&child.id)
                .expect("child exists")
                .subtasks
                .len(),
            1
        );
        let other = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "other".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        assert!(!supervisor.shares_root_with(&parent.id, &other.id)?);
        assert_eq!(
            supervisor
                .root_tree_records(&parent.id)?
                .into_iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![parent.id.clone(), child.id.clone()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn orchestrator_submits_input_and_resumes_pending_approvals() -> Result<()> {
        let (orchestrator, _) =
            orchestrator_fixture(ScriptedProvider(Mutex::new(VecDeque::from(vec![
                Ok(vec![
                    ModelStreamEvent::MessageId {
                        value: "assistant-1".to_string(),
                    },
                    ModelStreamEvent::TextDelta {
                        text: "Let me check.".to_string(),
                    },
                    ModelStreamEvent::ToolCall {
                        call: kheish_types::ToolCallRecord {
                            id: "call-1".to_string(),
                            name: "echo".to_string(),
                            input: json!({"text": "hello"}),
                            assistant_message_id: Some("assistant-1".to_string()),
                            assistant_provider_response_id: None,
                        },
                    },
                    ModelStreamEvent::Stop {
                        reason: kheish_types::ModelFinishReason::ToolCalls,
                    },
                ]),
                Ok(vec![
                    ModelStreamEvent::MessageId {
                        value: "assistant-2".to_string(),
                    },
                    ModelStreamEvent::TextDelta {
                        text: "Done.".to_string(),
                    },
                    ModelStreamEvent::Stop {
                        reason: kheish_types::ModelFinishReason::Completed,
                    },
                ]),
            ]))));
        let snapshot = orchestrator
            .spawn_root(ConversationKey {
                session_id: "root".to_string(),
                thread_id: None,
            })
            .await?;
        let pending = orchestrator
            .submit_input(
                &snapshot.agent.id,
                InputEnvelope::text("memory", "test", "root", "user-1", "run"),
                ModelGenerationConfig::default(),
            )
            .await?;
        assert_eq!(pending.pending_approvals.len(), 1);
        assert_eq!(pending.agent.status, AgentStatus::WaitingForApproval);

        let completed = orchestrator
            .resume_approvals(
                &snapshot.agent.id,
                vec![ApprovalResolution {
                    request_id: pending.pending_approvals[0].id.clone(),
                    behavior: kheish_types::ApprovalResolutionBehavior::Allow,
                    updated_input: None,
                    justification: Some("approved".to_string()),
                    reason: None,
                }],
            )
            .await?;
        assert!(completed.pending_approvals.is_empty());
        assert_eq!(completed.agent.status, AgentStatus::Idle);
        assert_eq!(completed.last_assistant_message.as_deref(), Some("Done."));
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_forks_and_restores_agents() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer.clone());
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.fork(
            parent.id.clone(),
            ConversationKey {
                session_id: "child-sidechain".to_string(),
                thread_id: Some("thread-1".to_string()),
            },
            ForkContext {
                parent_assistant_message: "Parent answer".to_string(),
                inherited_tool_call_ids: vec!["call-1".to_string()],
                team_name: None,
                isolation: None,
                system_prompt: "system".to_string(),
                prompt_merge_mode: PromptMergeMode::Replace,
                provider: None,
                generation: None,
                tool_surface: ToolSurfaceFilter::default(),
                worktree_path: Some("/tmp/worktree".to_string()),
            },
            Some("child_sidechain"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
            None,
        )?;
        supervisor.set_status(&child.id, AgentStatus::WaitingForApproval)?;
        let resumed = supervisor.resume(&child.id)?;
        assert_eq!(resumed.status, AgentStatus::Running);
        assert_eq!(
            supervisor.snapshot(),
            AgentSupervisor::restore(supervisor.snapshot(), observer).snapshot()
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_validates_repairs_and_audits_topology() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer.clone());
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.spawn(
            Some(parent.id.clone()),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: None,
            },
            Some("review"),
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        supervisor.set_status(&child.id, AgentStatus::Running)?;
        supervisor.set_nickname(&child.id, Some("Reviewer"))?;
        supervisor.post(MailboxMessage::new(
            "mailbox-test-2".to_string(),
            parent.id.clone(),
            child.id.clone(),
            "work".to_string(),
            json!({"hello": true}),
            0,
            None,
        ));
        supervisor.record_terminal_snapshot(ManagedAgentSnapshot {
            agent: child.clone(),
            pending_approvals: Vec::new(),
            pending_questions: Vec::new(),
            last_assistant_message: None,
            journal_len: 0,
            checkpoint_len: 0,
            last_error: None,
        });
        supervisor.validate_topology()?;

        let mut snapshot = supervisor.snapshot();
        snapshot.next_id = 0;
        snapshot.mailboxes.insert(
            AgentId("agent-404".to_string()),
            vec![MailboxMessage::new(
                "mailbox-stale".to_string(),
                parent.id.clone(),
                AgentId("agent-404".to_string()),
                "stale".to_string(),
                json!({}),
                0,
                None,
            )],
        );
        snapshot.terminal_snapshots.insert(
            AgentId("agent-404".to_string()),
            ManagedAgentSnapshot {
                agent: child.clone(),
                pending_approvals: Vec::new(),
                pending_questions: Vec::new(),
                last_assistant_message: None,
                journal_len: 0,
                checkpoint_len: 0,
                last_error: None,
            },
        );

        let restored = AgentSupervisor::try_restore(snapshot, observer)?;
        restored.validate_topology()?;
        assert_eq!(restored.mailbox_counts().get(&child.id), Some(&1));
        assert!(
            !restored
                .mailbox_counts()
                .contains_key(&AgentId("agent-404".to_string()))
        );
        assert!(
            restored
                .terminal_snapshot(&AgentId("agent-404".to_string()))
                .is_none()
        );
        let next = restored.spawn(
            None,
            ConversationKey {
                session_id: "next".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        assert_eq!(next.id.0, "agent-3");

        let audit = restored.audit_log(None);
        assert!(audit.iter().any(|entry| entry.event == "spawned"));
        assert!(audit.iter().any(|entry| entry.event == "status_changed"));
        assert!(audit.iter().any(|entry| entry.event == "nickname_updated"));
        assert!(
            audit
                .iter()
                .any(|entry| entry.event == "terminal_snapshot_recorded")
        );
        assert!(
            audit
                .windows(2)
                .all(|window| window[0].audit_id < window[1].audit_id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_rejects_corrupt_topology_snapshots() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer.clone());
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let child = supervisor.spawn(
            Some(parent.id.clone()),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;

        let mut missing_parent = supervisor.snapshot();
        missing_parent.agents.remove(&parent.id);
        assert!(AgentSupervisor::try_restore(missing_parent, observer.clone()).is_err());

        let mut cycle = supervisor.snapshot();
        cycle
            .agents
            .get_mut(&parent.id)
            .expect("parent exists")
            .parent = Some(child.id.clone());
        assert!(AgentSupervisor::try_restore(cycle, observer.clone()).is_err());

        let mut duplicate_session = supervisor.snapshot();
        duplicate_session
            .agents
            .get_mut(&child.id)
            .expect("child exists")
            .conversation
            .session_id = parent.conversation.session_id.clone();
        assert!(AgentSupervisor::try_restore(duplicate_session, observer.clone()).is_err());

        let mut mismatched_sidechain = supervisor.snapshot();
        mismatched_sidechain
            .agents
            .get_mut(&child.id)
            .expect("child exists")
            .sidechain_session_id = Some("wrong-session".to_string());
        assert!(AgentSupervisor::try_restore(mismatched_sidechain, observer).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_surfaces_audit_sink_failures_in_status() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer);
        supervisor.set_audit_sink(Arc::new(FailingAuditSink));

        let _ = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "audit-failure".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;

        let status = supervisor.status_snapshot();
        assert_eq!(status.audit_sink_error_count, 1);
        assert_eq!(
            status.last_audit_sink_error.as_deref(),
            Some("audit sink unavailable")
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_fuzzes_tree_helpers_with_deterministic_lcg() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer.clone());
        let mut seed = 0x5eed_u64;
        let mut ids: Vec<AgentId> = Vec::new();
        for index in 0..64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let parent = if ids.is_empty() || seed % 5 == 0 {
                None
            } else {
                Some(ids[(seed as usize) % ids.len()].clone())
            };
            let record = supervisor.spawn(
                parent,
                ConversationKey {
                    session_id: format!("agent-session-{index}"),
                    thread_id: None,
                },
                Some(&format!("agent {index}")),
                None,
                ChildRetentionPolicy::Retain,
                None,
                None,
            )?;
            ids.push(record.id);
        }

        supervisor.validate_topology()?;
        let restored = AgentSupervisor::try_restore(supervisor.snapshot(), observer)?;
        restored.validate_topology()?;
        for id in ids {
            let tree = restored.root_tree_records(&id)?;
            assert!(tree.iter().any(|record| record.id == id));
            let root_count = tree.iter().filter(|record| record.parent.is_none()).count();
            assert_eq!(root_count, 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_updates_agent_nicknames_and_terminal_snapshots() -> Result<()> {
        let observer = InMemoryObserver::shared();
        let supervisor = AgentSupervisor::new(observer);
        let alpha = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "alpha".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        let beta = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "beta".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;

        let alpha_named = supervisor.set_nickname(&alpha.id, Some("Reviewer"))?;
        assert_eq!(alpha_named.nickname.as_deref(), Some("Reviewer"));

        let beta_named = supervisor.set_nickname(&beta.id, Some("Reviewer"))?;
        assert_eq!(beta_named.nickname.as_deref(), Some("Reviewer 2"));

        let mut terminal_snapshot = ManagedAgentSnapshot {
            agent: beta_named.clone(),
            pending_approvals: Vec::new(),
            pending_questions: Vec::new(),
            last_assistant_message: None,
            journal_len: 0,
            checkpoint_len: 0,
            last_error: None,
        };
        terminal_snapshot.agent.closed_at_ms = Some(crate::supervisor::now_ms());
        supervisor.record_terminal_snapshot(terminal_snapshot);

        let beta_renamed = supervisor.set_nickname(&beta.id, Some("Infra Reviewer"))?;
        assert_eq!(beta_renamed.nickname.as_deref(), Some("Infra Reviewer"));
        let closed_snapshot = supervisor
            .terminal_snapshot(&beta.id)
            .expect("terminal snapshot should remain present");
        assert_eq!(
            closed_snapshot.agent.nickname.as_deref(),
            Some("Infra Reviewer")
        );

        let cleared = supervisor.set_nickname(&alpha.id, None)?;
        assert_eq!(cleared.nickname, None);
        Ok(())
    }

    #[tokio::test]
    async fn orchestrator_rejects_sidechain_workspace_escape() -> Result<()> {
        let (orchestrator, supervisor) =
            orchestrator_fixture(ScriptedProvider(Mutex::new(VecDeque::new())));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;

        let error = orchestrator
            .spawn_sidechain(
                &parent.id,
                ConversationKey {
                    session_id: "child".to_string(),
                    thread_id: None,
                },
                ForkContext {
                    parent_assistant_message: String::new(),
                    inherited_tool_call_ids: Vec::new(),
                    team_name: None,
                    isolation: None,
                    system_prompt: String::new(),
                    prompt_merge_mode: PromptMergeMode::Replace,
                    provider: None,
                    generation: None,
                    tool_surface: ToolSurfaceFilter::default(),
                    worktree_path: Some("/".to_string()),
                },
                Some("child".to_string()),
                None,
                ChildRetentionPolicy::CloseOnSettle,
                None,
                None,
                None,
                None,
            )
            .await
            .expect_err("absolute workspace escape should fail");
        assert!(error.to_string().contains("escapes workspace root"));
        assert!(
            supervisor.children_of(&parent.id).is_empty(),
            "failed sidechain spawn must roll back the forked supervisor record"
        );
        assert_eq!(orchestrator.runtime_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn orchestrator_rejects_restoring_sidechain_workspace_escape() -> Result<()> {
        let (orchestrator, supervisor) =
            orchestrator_fixture(ScriptedProvider(Mutex::new(VecDeque::new())));
        let parent = supervisor.spawn(
            None,
            ConversationKey {
                session_id: "parent".to_string(),
                thread_id: None,
            },
            None,
            None,
            ChildRetentionPolicy::Retain,
            None,
            None,
        )?;
        supervisor.fork(
            parent.id.clone(),
            ConversationKey {
                session_id: "child".to_string(),
                thread_id: None,
            },
            ForkContext {
                parent_assistant_message: String::new(),
                inherited_tool_call_ids: Vec::new(),
                team_name: None,
                isolation: None,
                system_prompt: String::new(),
                prompt_merge_mode: PromptMergeMode::Replace,
                provider: None,
                generation: None,
                tool_surface: ToolSurfaceFilter::default(),
                worktree_path: Some("/".to_string()),
            },
            Some("child"),
            None,
            ChildRetentionPolicy::CloseOnSettle,
            None,
            None,
            None,
        )?;

        let error = orchestrator
            .restore_registered_agents()
            .await
            .expect_err("restoring an escaped workspace should fail");
        assert!(error.to_string().contains("escapes workspace root"));
        Ok(())
    }
}
