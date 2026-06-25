use std::collections::BTreeSet;

use anyhow::{Result, bail};
use kheish_types::{
    ApprovalRequest, ApprovalResolution, ApprovalResolutionBehavior, PendingToolBatch,
    PendingToolDecision, PermissionDecision,
};

/// Applies caller-provided approval resolutions to one suspended tool batch.
pub fn apply_approval_resolutions(
    mut batch: PendingToolBatch,
    resolutions: &[ApprovalResolution],
) -> Result<PendingToolBatch> {
    let pending_ids = batch
        .decisions
        .iter()
        .filter_map(|entry| match &entry.decision {
            PermissionDecision::Ask { request } => Some(request.id.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut seen_resolution_ids = BTreeSet::new();
    for resolution in resolutions {
        if !seen_resolution_ids.insert(resolution.request_id.as_str()) {
            bail!(
                "duplicate approval resolution for request {}",
                resolution.request_id
            );
        }
        if !pending_ids.contains(resolution.request_id.as_str()) {
            bail!(
                "approval resolution references unknown pending request {}",
                resolution.request_id
            );
        }
    }

    for resolution in resolutions {
        let Some(decision) = batch.decisions.iter_mut().find(|entry| {
            matches!(
                &entry.decision,
                PermissionDecision::Ask { request } if request.id == resolution.request_id
            )
        }) else {
            continue;
        };

        if let Some(updated_input) = resolution.updated_input.clone() {
            decision.call.input = updated_input;
        }
        decision.decision = match resolution.behavior {
            ApprovalResolutionBehavior::Allow => PermissionDecision::Allow,
            ApprovalResolutionBehavior::Deny => PermissionDecision::Deny {
                reason: resolution
                    .reason
                    .clone()
                    .unwrap_or_else(|| "approval denied".to_string()),
            },
        };
    }
    Ok(batch)
}

/// Returns approval requests still waiting inside a pending tool decision set.
pub fn pending_approval_requests_from_decisions(
    decisions: &[PendingToolDecision],
) -> Vec<ApprovalRequest> {
    decisions
        .iter()
        .filter_map(|decision| match &decision.decision {
            PermissionDecision::Ask { request } => Some(request.clone()),
            _ => None,
        })
        .collect()
}

/// Returns approval requests still waiting inside a pending tool batch.
pub fn pending_approval_requests(batch: &PendingToolBatch) -> Vec<ApprovalRequest> {
    pending_approval_requests_from_decisions(&batch.decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pending_batch() -> PendingToolBatch {
        PendingToolBatch {
            turn: 1,
            assistant_message_id: "assistant-1".to_string(),
            decisions: vec![
                PendingToolDecision {
                    call: kheish_types::ToolCallRecord {
                        id: "call-1".to_string(),
                        name: "bash".to_string(),
                        input: json!({"command":"echo old"}),
                        assistant_message_id: Some("assistant-1".to_string()),
                        assistant_provider_response_id: None,
                    },
                    decision: PermissionDecision::Ask {
                        request: ApprovalRequest {
                            id: "approval-1".to_string(),
                            tool_call_id: "call-1".to_string(),
                            tool_name: "bash".to_string(),
                            input: json!({"command":"echo old"}),
                            scope: "default".to_string(),
                            reason: "review".to_string(),
                        },
                    },
                    hook_contexts: Vec::new(),
                    retry: false,
                },
                PendingToolDecision {
                    call: kheish_types::ToolCallRecord {
                        id: "call-2".to_string(),
                        name: "write_file".to_string(),
                        input: json!({"path":"a.txt"}),
                        assistant_message_id: Some("assistant-1".to_string()),
                        assistant_provider_response_id: None,
                    },
                    decision: PermissionDecision::Ask {
                        request: ApprovalRequest {
                            id: "approval-2".to_string(),
                            tool_call_id: "call-2".to_string(),
                            tool_name: "write_file".to_string(),
                            input: json!({"path":"a.txt"}),
                            scope: "default".to_string(),
                            reason: "review".to_string(),
                        },
                    },
                    hook_contexts: Vec::new(),
                    retry: false,
                },
            ],
        }
    }

    #[test]
    fn approval_resolutions_reduce_pending_batch_without_dropping_unresolved_requests() {
        let batch = apply_approval_resolutions(
            pending_batch(),
            &[ApprovalResolution {
                request_id: "approval-1".to_string(),
                behavior: ApprovalResolutionBehavior::Allow,
                updated_input: Some(json!({"command":"echo new"})),
                justification: None,
                reason: None,
            }],
        )
        .expect("apply approvals");

        assert!(matches!(
            batch.decisions[0].decision,
            PermissionDecision::Allow
        ));
        assert_eq!(batch.decisions[0].call.input, json!({"command":"echo new"}));
        assert_eq!(
            pending_approval_requests(&batch)
                .into_iter()
                .map(|request| request.id)
                .collect::<Vec<_>>(),
            vec!["approval-2"]
        );
    }

    #[test]
    fn approval_resolutions_can_clear_all_pending_requests() {
        let batch = apply_approval_resolutions(
            pending_batch(),
            &[
                ApprovalResolution {
                    request_id: "approval-1".to_string(),
                    behavior: ApprovalResolutionBehavior::Allow,
                    updated_input: None,
                    justification: None,
                    reason: None,
                },
                ApprovalResolution {
                    request_id: "approval-2".to_string(),
                    behavior: ApprovalResolutionBehavior::Deny,
                    updated_input: None,
                    justification: None,
                    reason: Some("not needed".to_string()),
                },
            ],
        )
        .expect("apply approvals");

        assert!(pending_approval_requests(&batch).is_empty());
        assert!(matches!(
            &batch.decisions[1].decision,
            PermissionDecision::Deny { reason } if reason == "not needed"
        ));
    }

    #[test]
    fn approval_resolutions_reject_duplicate_request_ids() {
        let error = apply_approval_resolutions(
            pending_batch(),
            &[
                ApprovalResolution {
                    request_id: "approval-1".to_string(),
                    behavior: ApprovalResolutionBehavior::Allow,
                    updated_input: None,
                    justification: None,
                    reason: None,
                },
                ApprovalResolution {
                    request_id: "approval-1".to_string(),
                    behavior: ApprovalResolutionBehavior::Deny,
                    updated_input: None,
                    justification: None,
                    reason: Some("duplicate".to_string()),
                },
            ],
        )
        .expect_err("duplicate approval id should fail");

        assert_eq!(
            error.to_string(),
            "duplicate approval resolution for request approval-1"
        );
    }

    #[test]
    fn approval_resolutions_reject_unknown_request_ids() {
        let error = apply_approval_resolutions(
            pending_batch(),
            &[ApprovalResolution {
                request_id: "approval-missing".to_string(),
                behavior: ApprovalResolutionBehavior::Allow,
                updated_input: None,
                justification: None,
                reason: None,
            }],
        )
        .expect_err("unknown approval id should fail");

        assert_eq!(
            error.to_string(),
            "approval resolution references unknown pending request approval-missing"
        );
    }
}
