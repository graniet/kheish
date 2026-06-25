//! Session goal workflow methods implemented on [`DaemonState`].

use kheish_types::{SessionGoal, SessionGoalStatus};
use serde_json::Value;

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn load_session_goal(&self, session_id: &str) -> Result<Option<SessionGoal>> {
        self.goal_service.load_session_goal(session_id).await
    }

    pub(crate) async fn session_goal_response(
        &self,
        session_id: &str,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        Ok(SessionGoalResponse::new(
            self.load_session_goal(session_id).await?,
        ))
    }

    pub(crate) async fn create_session_goal(
        &self,
        session_id: &str,
        objective: String,
        token_budget: Option<u64>,
        created_by_run_id: Option<String>,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        let goal = self
            .goal_service
            .create_session_goal(
                session_id,
                objective,
                token_budget,
                SessionGoalStatus::Active,
                created_by_run_id,
            )
            .await?;
        Ok(SessionGoalResponse::new(Some(goal)))
    }

    pub(crate) async fn create_session_goal_from_request(
        &self,
        session_id: &str,
        request: crate::SetSessionGoalRequest,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        let goal = self
            .goal_service
            .create_session_goal(
                session_id,
                request.objective,
                request.token_budget,
                request.status.unwrap_or(SessionGoalStatus::Active),
                None,
            )
            .await?;
        Ok(SessionGoalResponse::new(Some(goal)))
    }

    pub(crate) async fn set_session_goal(
        &self,
        session_id: &str,
        request: crate::SetSessionGoalRequest,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        let goal = self
            .goal_service
            .set_session_goal(
                session_id,
                request.objective,
                request.token_budget,
                request.status.unwrap_or(SessionGoalStatus::Active),
            )
            .await?;
        Ok(SessionGoalResponse::new(Some(goal)))
    }

    pub(crate) async fn patch_session_goal(
        &self,
        session_id: &str,
        request: crate::PatchSessionGoalRequest,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        validate_goal_patch_request(&request)?;
        if !goal_patch_mutates(&request) {
            let goal = self
                .load_session_goal(session_id)
                .await?
                .ok_or_else(|| anyhow!("session has no goal"))?;
            return Ok(SessionGoalResponse::new(Some(goal)));
        }
        let token_budget = if request.clear_token_budget.unwrap_or(false) {
            Some(None)
        } else {
            request.token_budget.map(Some)
        };
        if request.require_no_active_runs.unwrap_or(false) {
            self.run_service
                .with_session_idle_guard(session_id, || async {
                    self.goal_service
                        .update_session_goal(
                            session_id,
                            SessionGoalPatch {
                                objective: request.objective,
                                status: request.status,
                                token_budget,
                                expected_goal_id: request.expected_goal_id,
                                expected_version: request.expected_version,
                                completed_by_run_id: None,
                                ..SessionGoalPatch::default()
                            },
                        )
                        .await
                })
                .await
                .map(|goal| SessionGoalResponse::new(Some(goal)))
        } else {
            let goal = self
                .goal_service
                .update_session_goal(
                    session_id,
                    SessionGoalPatch {
                        objective: request.objective,
                        status: request.status,
                        token_budget,
                        expected_goal_id: request.expected_goal_id,
                        expected_version: request.expected_version,
                        completed_by_run_id: None,
                        ..SessionGoalPatch::default()
                    },
                )
                .await?;
            Ok(SessionGoalResponse::new(Some(goal)))
        }
    }

    pub(crate) async fn complete_session_goal_from_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        let run = self.run_service.get_run(run_id).await?;
        let goal = self
            .load_session_goal(session_id)
            .await?
            .ok_or_else(|| anyhow!("session has no goal"))?;
        let (expected_goal_id, expected_definition_version) =
            goal_completion_expectation_from_run(&goal, run.input_metadata.as_ref(), run_id)?;
        let goal = self
            .goal_service
            .update_session_goal(
                session_id,
                SessionGoalPatch {
                    status: Some(SessionGoalStatus::Complete),
                    expected_goal_id: Some(expected_goal_id),
                    expected_definition_version: Some(expected_definition_version),
                    completed_by_run_id: Some(run_id.to_string()),
                    ..SessionGoalPatch::default()
                },
            )
            .await?;
        Ok(SessionGoalResponse::new(Some(goal)))
    }

    pub(crate) async fn clear_session_goal(&self, session_id: &str) -> Result<SessionGoalResponse> {
        self.ensure_goal_session_exists(session_id).await?;
        self.goal_service.clear_session_goal(session_id).await?;
        Ok(SessionGoalResponse::new(None))
    }

    async fn ensure_goal_session_exists(&self, session_id: &str) -> Result<()> {
        self.agent_id_for_session(session_id).await.map(|_| ())
    }
}

fn goal_completion_expectation_from_run(
    goal: &SessionGoal,
    metadata: Option<&Value>,
    run_id: &str,
) -> Result<(String, u64)> {
    let (expected_goal_id, expected_version) = goal_expectation_from_run_metadata(metadata);
    if let (Some(expected_goal_id), Some(expected_version)) = (expected_goal_id, expected_version) {
        return Ok((expected_goal_id, expected_version));
    }
    if goal.created_by_run_id.as_deref() == Some(run_id) {
        return Ok((goal.goal_id.clone(), 1));
    }
    bail!("run is not bound to a session goal");
}

fn goal_expectation_from_run_metadata(metadata: Option<&Value>) -> (Option<String>, Option<u64>) {
    let Some(daemon_metadata) = metadata.and_then(|metadata| metadata.get("daemon")) else {
        return (None, None);
    };
    let expected_goal_id = daemon_metadata
        .get("goal_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expected_version = daemon_metadata.get("goal_version").and_then(Value::as_u64);
    (expected_goal_id, expected_version)
}

fn goal_patch_mutates(request: &crate::PatchSessionGoalRequest) -> bool {
    request.objective.is_some()
        || request.status.is_some()
        || request.token_budget.is_some()
        || request.clear_token_budget.unwrap_or(false)
}

fn validate_goal_patch_request(request: &crate::PatchSessionGoalRequest) -> Result<()> {
    if goal_patch_mutates(request)
        && (request.expected_goal_id.is_none() || request.expected_version.is_none())
    {
        bail!("session goal patch requires expected_goal_id and expected_version");
    }
    if matches!(request.status.as_ref(), Some(SessionGoalStatus::Complete))
        && !request.require_no_active_runs.unwrap_or(false)
    {
        bail!("session goal completion requires require_no_active_runs");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kheish_types::SessionGoalStatus;
    use serde_json::json;

    use super::*;

    fn goal() -> SessionGoal {
        SessionGoal {
            goal_id: "goal-1".to_string(),
            session_id: "session-1".to_string(),
            objective: "Ship it".to_string(),
            status: SessionGoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_ms: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            version: 3,
            definition_version: 2,
            created_by_run_id: Some("run-create".to_string()),
            accounted_usage: BTreeMap::new(),
            last_continuation_run_id: None,
            budget_limited_by_run_id: None,
            budget_wrapup_run_id: None,
            completed_by_run_id: None,
        }
    }

    #[test]
    fn goal_completion_expectation_allows_goal_created_by_same_run() {
        let expectation =
            goal_completion_expectation_from_run(&goal(), None, "run-create").unwrap();

        assert_eq!(expectation, ("goal-1".to_string(), 1));
    }

    #[test]
    fn goal_completion_expectation_prefers_run_metadata_binding() {
        let metadata = json!({
            "daemon": {
                "goal_id": "goal-bound",
                "goal_version": 7,
            }
        });

        let expectation =
            goal_completion_expectation_from_run(&goal(), Some(&metadata), "run-create").unwrap();

        assert_eq!(expectation, ("goal-bound".to_string(), 7));
    }

    #[test]
    fn goal_completion_expectation_rejects_unbound_run() {
        let error = goal_completion_expectation_from_run(&goal(), None, "run-other")
            .expect_err("unbound run should fail");

        assert_eq!(error.to_string(), "run is not bound to a session goal");
    }

    #[test]
    fn empty_goal_patch_is_not_mutating() {
        assert!(!goal_patch_mutates(
            &crate::PatchSessionGoalRequest::default()
        ));
    }
}
