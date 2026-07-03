//! Output persistence and delivery methods implemented on [`DaemonState`].

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn list_deliveries(
        &self,
        filter: crate::delivery::DeliveryListFilter,
    ) -> Result<Vec<crate::DeliveryView>> {
        self.delivery_service.list_deliveries(filter).await
    }

    pub(crate) async fn get_delivery(
        &self,
        delivery_id: &str,
    ) -> Result<Option<crate::DeliveryView>> {
        self.delivery_service.get_delivery(delivery_id).await
    }

    pub(crate) async fn replay_dead_letter_delivery(
        &self,
        delivery_id: &str,
        force: bool,
    ) -> Result<Option<crate::DeliveryReplayResponse>> {
        self.delivery_service
            .replay_dead_letter(delivery_id, force)
            .await
    }

    pub(crate) async fn resolve_dead_letter_delivery(
        &self,
        delivery_id: &str,
        reason: &str,
    ) -> Result<Option<crate::DeliveryView>> {
        self.delivery_service
            .resolve_dead_letter(delivery_id, reason)
            .await
    }

    pub(crate) async fn bulk_replay_dead_letter_deliveries(
        &self,
        filter: crate::delivery::DeliveryListFilter,
        force: bool,
        dry_run: bool,
        unresolved_only: bool,
        limit: Option<usize>,
    ) -> Result<crate::DeliveryBulkReplayResponse> {
        self.delivery_service
            .bulk_replay_dead_letters(filter, force, dry_run, unresolved_only, limit)
            .await
    }

    pub(crate) async fn reset_delivery_backpressure(
        &self,
        target: Option<&str>,
        plugin: Option<&str>,
        dry_run: bool,
    ) -> Result<crate::DeliveryBackpressureResetResponse> {
        self.delivery_service
            .reset_backpressure(target, plugin, dry_run)
            .await
    }

    pub(crate) async fn delivery_queue_status_snapshot(
        &self,
        now: u64,
    ) -> Result<crate::DeliveryQueueStatusView> {
        self.delivery_service.status_snapshot(now).await
    }

    pub(super) async fn attach_delivery_views_to_run(&self, run: &mut RunView) -> Result<()> {
        let deliveries = self
            .delivery_service
            .list_deliveries(crate::delivery::DeliveryListFilter {
                run_id: Some(run.run_id.clone()),
                ..Default::default()
            })
            .await?;
        run.deliveries = deliveries;
        Ok(())
    }

    pub(super) async fn attach_delivery_views_to_runs(
        &self,
        runs: &mut [RunView],
        session_id: Option<&str>,
    ) -> Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        let deliveries = self
            .delivery_service
            .list_deliveries(crate::delivery::DeliveryListFilter {
                session_id: session_id.map(ToOwned::to_owned),
                ..Default::default()
            })
            .await?;
        let mut by_run = BTreeMap::<String, Vec<crate::DeliveryView>>::new();
        for delivery in deliveries {
            if let Some(run_id) = delivery.run_id.clone() {
                by_run.entry(run_id).or_default().push(delivery);
            }
        }
        for run in runs {
            run.deliveries = by_run.remove(&run.run_id).unwrap_or_default();
        }
        Ok(())
    }

    pub(crate) async fn record_output(
        &self,
        output: DaemonOutputRecord,
        run_id: Option<&str>,
    ) -> Result<()> {
        let hook_run_id = run_id.map(ToString::to_string);
        let mut channel_scoped = false;
        if let Some(run_id) = run_id {
            let record = self.run_service.run_record(run_id).await?;
            channel_scoped = record.is_channel_delivery_lineage();
            if let Some(view) = self
                .run_service
                .record_output_for_run(run_id, output.clone())
                .await?
            {
                self.sync_project_tasks_for_run_or_warn(&view, "record_output")
                    .await;
                if view.status.is_terminal() {
                    let record = self.run_service.run_record(run_id).await?;
                    self.persist_run_memory_record(&record).await;
                }
            }
        }
        if channel_scoped {
            return Ok(());
        }
        self.delivery_service.append_output(&output).await?;
        self.events.publish(DaemonEvent::Output {
            output: output.clone(),
        });
        let _ = self
            .dispatch_daemon_hook(
                HookEventName::Notification,
                output.address.clone(),
                Some(output.session_id.clone()),
                None,
                hook_run_id,
                json!({ "output": output }),
            )
            .await;
        Ok(())
    }

    pub(super) async fn emit_session_output(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        output: RichOutput,
        override_targets: Option<Vec<ReplyHandle>>,
    ) -> Result<()> {
        let output = output.normalized();
        let run_reply_targets = match run_id {
            Some(run_id) => self.run_reply_targets(run_id).await,
            None => Vec::new(),
        };
        let session_reply_targets = self.session_reply_targets(session_id).await;
        let reply_targets = if let Some(override_targets) =
            override_targets.filter(|targets| !targets.is_empty())
        {
            normalize_reply_targets(None, override_targets)
        } else if !run_reply_targets.is_empty() {
            normalize_reply_targets(None, run_reply_targets)
        } else {
            normalize_reply_targets(None, session_reply_targets)
        };
        self.record_output(
            DaemonOutputRecord {
                session_id: session_id.to_string(),
                run_id: run_id.map(ToString::to_string),
                content: output.content.clone(),
                parts: output.parts.clone(),
                artifacts: output.artifacts.clone(),
                source_kind: Some(crate::DaemonOutputSourceKind::DaemonEmitOutput),
                plugin: Some("daemon".to_string()),
                address: Some(session_id.to_string()),
            },
            run_id,
        )
        .await?;
        if let Some(run_id) = run_id
            && self.run_record(run_id).await?.is_channel_delivery_lineage()
        {
            return Ok(());
        }
        if reply_targets.is_empty() {
            return Ok(());
        }

        let metadata = run_id
            .map(|value| json!({ "run_id": value }))
            .unwrap_or(Value::Null);
        let envelope = ResponseEnvelope {
            conversation: self.session_conversation_key(session_id).await?,
            reply_targets: reply_targets.clone(),
            reply: reply_targets.first().cloned(),
            content: output.content,
            parts: output.parts,
            artifacts: output.artifacts,
            metadata: match metadata {
                Value::Object(mut map) => {
                    map.insert(
                        "output_kind".to_string(),
                        Value::String("daemon_emit_output".to_string()),
                    );
                    Value::Object(map)
                }
                Value::Null => json!({ "output_kind": "daemon_emit_output" }),
                other => json!({
                    "output_kind": "daemon_emit_output",
                    "metadata": other,
                }),
            },
        };
        if let Err(error) = self.delivery_service.deliver(envelope).await {
            warn!(
                session_id,
                run_id = run_id.unwrap_or("<none>"),
                error = ?error,
                "daemon-generated output delivery failed after local persistence"
            );
        }
        Ok(())
    }

    pub(super) async fn emit_operator_notification(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        request: crate::control_tools::OperatorNotificationRequest,
    ) -> Result<crate::control_tools::OperatorNotificationToolResponse> {
        let targets = self.operator_notification_reply_targets(session_id).await?;
        anyhow::ensure!(
            !targets.is_empty(),
            "operator notification requires at least one configured session reply target"
        );
        let message = format_operator_notification_message(&request)?;
        let output = RichOutput::text(message);
        let mut metadata = json!({
            "output_kind": "operator_notification",
            "run_id": run_id,
            "urgency": request.urgency,
        });
        if let Some(idempotency_key) = request.idempotency_key.as_deref()
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert(
                "delivery_idempotency_key".to_string(),
                serde_json::Value::String(idempotency_key.to_string()),
            );
        }
        let envelope = ResponseEnvelope {
            conversation: self.session_conversation_key(session_id).await?,
            reply_targets: targets.clone(),
            reply: targets.first().cloned(),
            content: output.content.clone(),
            parts: output.parts.clone(),
            artifacts: output.artifacts.clone(),
            metadata,
        };
        self.delivery_service.deliver(envelope).await?;
        self.record_output(
            DaemonOutputRecord {
                session_id: session_id.to_string(),
                run_id: run_id.map(ToString::to_string),
                content: output.content.clone(),
                parts: output.parts.clone(),
                artifacts: output.artifacts.clone(),
                source_kind: Some(crate::DaemonOutputSourceKind::OperatorNotification),
                plugin: Some("daemon".to_string()),
                address: Some(session_id.to_string()),
            },
            run_id,
        )
        .await?;
        Ok(crate::control_tools::OperatorNotificationToolResponse {
            queued: true,
            target_count: targets.len(),
            session_id: session_id.to_string(),
            run_id: run_id.map(ToString::to_string),
            output_kind: "operator_notification".to_string(),
        })
    }
}

fn format_operator_notification_message(
    request: &crate::control_tools::OperatorNotificationRequest,
) -> Result<String> {
    let message = request.message.trim();
    anyhow::ensure!(
        !message.is_empty(),
        "operator notification message is required"
    );
    let mut lines = Vec::new();
    if let Some(subject) = request
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Subject: {subject}"));
    }
    if let Some(urgency) = request
        .urgency
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Urgency: {urgency}"));
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(message.to_string());
    Ok(lines.join("\n"))
}
