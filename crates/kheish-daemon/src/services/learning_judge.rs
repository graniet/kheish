//! Model-backed review helpers for daemon-owned learning automation.

use std::sync::Arc;

use anyhow::Result;
use kheish_types::{HookModelConfig, StructuredFieldSchema, StructuredValueKind};
use serde::Deserialize;
use serde_json::json;

use crate::hooks::DaemonHookDispatcher;
use crate::learning::{
    LearningAutomationMode, LearningAutomationPolicyConfig, LearningCandidateView,
    LearningJudgeReview, LearningPublicationAction,
};
use crate::services::LearningAutomationEvaluation;

const DEFAULT_LEARNING_JUDGE_TIMEOUT_MS: u64 = 15_000;
const MAX_JUDGE_REASON_CHARS: usize = 400;

/// Executes model-backed reviews for daemon-owned learning automation.
#[derive(Clone)]
pub(crate) struct LearningJudgeService {
    hooks: Arc<DaemonHookDispatcher>,
}

impl LearningJudgeService {
    /// Creates a new judge service bound to the daemon hook/model runtime.
    pub(crate) fn new(hooks: Arc<DaemonHookDispatcher>) -> Self {
        Self { hooks }
    }

    /// Applies the configured model-backed judge to one policy evaluation when enabled.
    pub(crate) async fn apply_if_enabled(
        &self,
        candidate: &LearningCandidateView,
        evaluation: LearningAutomationEvaluation,
        settings: &LearningAutomationPolicyConfig,
        reviewed_at_ms: u64,
    ) -> Result<LearningAutomationEvaluation> {
        if !settings.judge.enabled
            || matches!(evaluation.action, LearningPublicationAction::ManualReview)
        {
            return Ok(evaluation);
        }

        match self
            .judge_candidate(
                candidate,
                &evaluation,
                settings.judge.model.as_ref(),
                settings.judge.timeout_ms,
                reviewed_at_ms,
            )
            .await
        {
            Ok(verdict) => Ok(apply_judge_verdict(evaluation, verdict, reviewed_at_ms)),
            Err(error) => Ok(handle_judge_failure(
                evaluation,
                reviewed_at_ms,
                error.to_string(),
            )),
        }
    }

    async fn judge_candidate(
        &self,
        candidate: &LearningCandidateView,
        evaluation: &LearningAutomationEvaluation,
        model: Option<&HookModelConfig>,
        timeout_ms: Option<u64>,
        reviewed_at_ms: u64,
    ) -> Result<LearningJudgeVerdict> {
        let prompt = build_judge_prompt(candidate, evaluation);
        let payload = self
            .hooks
            .run_structured_prompt_json(
                candidate.source.session_id.as_deref(),
                candidate.source.run_id.as_deref(),
                &prompt,
                Some(LEARNING_JUDGE_SYSTEM_PROMPT),
                model,
                timeout_ms.or(Some(DEFAULT_LEARNING_JUDGE_TIMEOUT_MS)),
                learning_judge_schema(),
            )
            .await?;
        let mut verdict: LearningJudgeVerdict = serde_json::from_value(payload)?;
        verdict.action = clamp_judge_action(evaluation.action.clone(), verdict.action);
        verdict.reason = normalize_judge_reason(verdict.reason);
        verdict.reviewed_at_ms = reviewed_at_ms;
        Ok(verdict)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct LearningJudgeVerdict {
    action: LearningPublicationAction,
    reason: String,
    #[serde(skip)]
    reviewed_at_ms: u64,
}

fn apply_judge_verdict(
    mut evaluation: LearningAutomationEvaluation,
    verdict: LearningJudgeVerdict,
    reviewed_at_ms: u64,
) -> LearningAutomationEvaluation {
    let judge_review = LearningJudgeReview {
        action: verdict.action.clone(),
        judged_at_ms: reviewed_at_ms,
        reason: verdict.reason.clone(),
    };
    let judge_summary = if verdict.action == evaluation.action {
        format!("judge confirmed {:?}: {}", verdict.action, verdict.reason)
    } else {
        format!(
            "judge changed action from {:?} to {:?}: {}",
            evaluation.action, verdict.action, verdict.reason
        )
    };
    evaluation.action = verdict.action;
    evaluation.reason = format!("{}; {}", evaluation.reason, judge_summary);
    evaluation.judge_review = Some(judge_review);
    evaluation
}

fn handle_judge_failure(
    mut evaluation: LearningAutomationEvaluation,
    reviewed_at_ms: u64,
    error: String,
) -> LearningAutomationEvaluation {
    let reason = normalize_judge_reason(format!("judge execution failed: {error}"));
    evaluation.judge_review = Some(LearningJudgeReview {
        action: LearningPublicationAction::ManualReview,
        judged_at_ms: reviewed_at_ms,
        reason: reason.clone(),
    });
    if evaluation.mode == LearningAutomationMode::Enabled {
        evaluation.action = LearningPublicationAction::ManualReview;
        evaluation.reason = format!("{}; {}", evaluation.reason, reason);
    }
    evaluation
}

fn build_judge_prompt(
    candidate: &LearningCandidateView,
    evaluation: &LearningAutomationEvaluation,
) -> String {
    let allowed_actions = allowed_judge_actions(&evaluation.action)
        .into_iter()
        .map(action_name)
        .collect::<Vec<_>>();
    let payload = json!({
        "candidate": candidate,
        "baseline": {
            "mode": evaluation.mode,
            "action": evaluation.action,
            "matched_rule_name": evaluation.matched_rule_name,
            "reason": evaluation.reason,
        },
        "allowed_actions": allowed_actions,
        "instructions": [
            "Review whether the candidate should keep the baseline automatic action.",
            "Be conservative. When uncertain, choose manual_review.",
            "Only choose reject when the candidate clearly should not become durable memory.",
            "Never assume facts that are not present in the candidate or its source metadata.",
            "Return only JSON matching the requested schema."
        ]
    });
    serde_json::to_string_pretty(&payload).expect("learning judge prompt serialization must work")
}

fn allowed_judge_actions(baseline: &LearningPublicationAction) -> Vec<LearningPublicationAction> {
    match baseline {
        LearningPublicationAction::ManualReview => vec![LearningPublicationAction::ManualReview],
        LearningPublicationAction::Reject => vec![
            LearningPublicationAction::Reject,
            LearningPublicationAction::ManualReview,
        ],
        LearningPublicationAction::PublishProvisional => vec![
            LearningPublicationAction::Reject,
            LearningPublicationAction::ManualReview,
            LearningPublicationAction::PublishProvisional,
        ],
        LearningPublicationAction::PublishActive => vec![
            LearningPublicationAction::Reject,
            LearningPublicationAction::ManualReview,
            LearningPublicationAction::PublishProvisional,
            LearningPublicationAction::PublishActive,
        ],
    }
}

fn clamp_judge_action(
    baseline: LearningPublicationAction,
    judged: LearningPublicationAction,
) -> LearningPublicationAction {
    match baseline {
        LearningPublicationAction::ManualReview => LearningPublicationAction::ManualReview,
        LearningPublicationAction::Reject => match judged {
            LearningPublicationAction::Reject | LearningPublicationAction::ManualReview => judged,
            LearningPublicationAction::PublishProvisional
            | LearningPublicationAction::PublishActive => LearningPublicationAction::ManualReview,
        },
        LearningPublicationAction::PublishProvisional => match judged {
            LearningPublicationAction::PublishActive => {
                LearningPublicationAction::PublishProvisional
            }
            other => other,
        },
        LearningPublicationAction::PublishActive => judged,
    }
}

fn action_name(action: LearningPublicationAction) -> &'static str {
    match action {
        LearningPublicationAction::ManualReview => "manual_review",
        LearningPublicationAction::Reject => "reject",
        LearningPublicationAction::PublishProvisional => "publish_provisional",
        LearningPublicationAction::PublishActive => "publish_active",
    }
}

fn normalize_judge_reason(reason: impl Into<String>) -> String {
    let normalized = reason.into().trim().replace(char::is_whitespace, " ");
    let compact = normalized
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = compact.trim();
    if trimmed.is_empty() {
        return "judge did not provide a reason".to_string();
    }
    trimmed.chars().take(MAX_JUDGE_REASON_CHARS).collect()
}

fn learning_judge_schema() -> StructuredFieldSchema {
    let mut schema = StructuredFieldSchema::new(StructuredValueKind::Object);
    schema.fields.insert(
        "action".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema.fields.insert(
        "reason".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    schema
}

const LEARNING_JUDGE_SYSTEM_PROMPT: &str = r#"You are Kheish's daemon-owned learning judge.

You review one durable-memory candidate after deterministic policy evaluation.
You do not create new facts.
You do not broaden automation beyond the allowed actions.
When uncertain, prefer manual_review.
Return only one JSON object with:
- action: one allowed action string
- reason: one concise audit reason"#;

#[cfg(test)]
mod tests {
    use super::{LearningPublicationAction, clamp_judge_action, normalize_judge_reason};

    #[test]
    fn learning_judge_clamps_actions_to_the_policy_envelope() {
        assert_eq!(
            clamp_judge_action(
                LearningPublicationAction::PublishProvisional,
                LearningPublicationAction::PublishActive
            ),
            LearningPublicationAction::PublishProvisional
        );
        assert_eq!(
            clamp_judge_action(
                LearningPublicationAction::Reject,
                LearningPublicationAction::PublishProvisional
            ),
            LearningPublicationAction::ManualReview
        );
        assert_eq!(
            clamp_judge_action(
                LearningPublicationAction::PublishActive,
                LearningPublicationAction::Reject
            ),
            LearningPublicationAction::Reject
        );
    }

    #[test]
    fn learning_judge_normalizes_and_bounds_reasons() {
        let normalized = normalize_judge_reason("  too   noisy\nfor\tactive  ");
        assert_eq!(normalized, "too noisy for active");
        let empty = normalize_judge_reason(" \n\t ");
        assert_eq!(empty, "judge did not provide a reason");
    }
}
