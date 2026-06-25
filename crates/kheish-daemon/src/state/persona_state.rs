//! Persona and session-persona persistence methods implemented on [`DaemonState`].

use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

fn bind_persona_snapshot(
    record: &PersonaRecord,
    default_inline_skills: Vec<kheish_types::ActiveSkillSnapshot>,
    bound_at_ms: u64,
) -> kheish_types::SessionPersonaBinding {
    kheish_types::SessionPersonaBinding {
        persona_id: record.persona_id.clone(),
        persona_version: record.version,
        display_name: record.display_name.clone(),
        soul: record.soul.clone(),
        soul_sha256: hex::encode(Sha256::digest(record.soul.as_bytes())),
        capability_scope: record.capability_scope.clone(),
        default_inline_skills,
        bound_at_ms,
    }
}

fn normalize_persona_skill_assignments(
    assignments: &[kheish_types::PersonaSkillAssignment],
) -> Result<Vec<kheish_types::PersonaSkillAssignment>> {
    let mut normalized = Vec::with_capacity(assignments.len());
    let mut seen = BTreeSet::new();
    for assignment in assignments {
        let name = assignment.name.trim();
        anyhow::ensure!(
            !name.is_empty(),
            "persona skill assignment name cannot be empty"
        );
        let args = assignment
            .args
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        anyhow::ensure!(
            seen.insert(name.to_string()),
            "persona skill `{name}` is assigned more than once"
        );
        normalized.push(kheish_types::PersonaSkillAssignment {
            name: name.to_string(),
            args,
        });
    }
    Ok(normalized)
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn list_persona_records(&self) -> Vec<PersonaIndexEntry> {
        self.persona_service.list_personas().await
    }

    pub(crate) async fn repair_session_persona_index(&self) -> Result<()> {
        self.session_service.repair_session_persona_index().await
    }

    pub(crate) async fn prune_session_persona_index(&self) -> Result<()> {
        self.session_service.prune_session_persona_index().await
    }

    pub(crate) async fn repair_session_persona_index_for_sessions<I>(
        &self,
        session_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        self.session_service
            .repair_session_persona_index_for_sessions(session_ids)
            .await
    }

    pub(crate) async fn get_persona_record(&self, persona_id: &str) -> Result<PersonaRecord> {
        self.persona_service.get_persona(persona_id).await
    }

    pub(crate) async fn create_persona_record(
        &self,
        persona_id: Option<String>,
        display_name: String,
        soul: String,
        capability_scope: kheish_types::CapabilityScope,
        default_skills: Vec<kheish_types::PersonaSkillAssignment>,
        metadata: Value,
    ) -> Result<PersonaRecord> {
        let capability_scope = capability_scope.normalized();
        let default_skills = normalize_persona_skill_assignments(&default_skills)?;
        self.validate_persona_default_skills(&capability_scope, &default_skills)
            .await?;
        let timestamp_ms = now_ms();
        let record = PersonaRecord {
            persona_id: persona_id.unwrap_or_else(|| self.persona_service.next_persona_id()),
            display_name,
            soul,
            capability_scope,
            default_skills,
            version: 1,
            created_at_ms: timestamp_ms,
            updated_at_ms: timestamp_ms,
            metadata,
        };
        self.persona_service.create_persona(record).await
    }

    pub(crate) async fn update_persona_record(
        &self,
        persona_id: &str,
        display_name: Option<String>,
        soul: Option<String>,
        capability_scope: Option<kheish_types::CapabilityScope>,
        default_skills: Option<Vec<kheish_types::PersonaSkillAssignment>>,
        metadata: Option<Value>,
    ) -> Result<PersonaRecord> {
        let current = self.get_persona_record(persona_id).await?;
        let capability_scope = capability_scope.map(|scope| scope.normalized());
        let default_skills = default_skills
            .map(|assignments| normalize_persona_skill_assignments(&assignments))
            .transpose()?;
        let final_capability_scope = capability_scope
            .clone()
            .unwrap_or_else(|| current.capability_scope.clone());
        let final_default_skills = default_skills
            .clone()
            .unwrap_or_else(|| current.default_skills.clone());
        self.validate_persona_default_skills(&final_capability_scope, &final_default_skills)
            .await?;
        self.persona_service
            .update_persona(persona_id, |record| {
                let mut changed = false;
                if let Some(display_name) = display_name.clone()
                    && record.display_name != display_name
                {
                    record.display_name = display_name;
                    changed = true;
                }
                if let Some(soul) = soul.clone()
                    && record.soul != soul
                {
                    record.soul = soul;
                    changed = true;
                }
                if let Some(capability_scope) = capability_scope.clone()
                    && record.capability_scope != capability_scope
                {
                    record.capability_scope = capability_scope;
                    changed = true;
                }
                if let Some(default_skills) = default_skills.clone()
                    && record.default_skills != default_skills
                {
                    record.default_skills = default_skills;
                    changed = true;
                }
                if let Some(metadata) = metadata.clone()
                    && record.metadata != metadata
                {
                    record.metadata = metadata;
                    changed = true;
                }
                if changed {
                    record.version = record.version.saturating_add(1);
                    record.updated_at_ms = now_ms();
                }
                Ok(changed)
            })
            .await
    }

    pub(crate) async fn load_session_persona_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<kheish_types::SessionPersonaBinding>> {
        self.session_service
            .load_session_persona_binding(session_id)
            .await
    }

    pub(crate) async fn bind_session_persona(
        &self,
        session_id: &str,
        persona_id: &str,
    ) -> Result<kheish_types::SessionPersonaBinding> {
        self.ensure_session_persona_idle(session_id).await?;
        self.agent_id_for_session(session_id).await?;
        let persona = self.get_persona_record(persona_id).await?;
        self.persist_session_persona_binding(session_id, &persona)
            .await
    }

    pub(crate) async fn clear_session_persona(&self, session_id: &str) -> Result<()> {
        self.ensure_session_persona_idle(session_id).await?;
        self.agent_id_for_session(session_id).await?;
        let previous_binding = self.load_session_persona_binding(session_id).await?;
        self.session_service
            .save_session_persona_binding(session_id, None)
            .await?;
        if let Err(error) = self
            .session_service
            .forget_session_persona(session_id)
            .await
        {
            return match self
                .session_service
                .save_session_persona_binding(session_id, previous_binding.as_ref())
                .await
            {
                Ok(_) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist session persona cache update for {session_id}; metadata rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(())
    }

    pub(crate) async fn set_session_persona_view(
        &self,
        session_id: &str,
        persona_id: &str,
    ) -> Result<SessionView> {
        self.bind_session_persona(session_id, persona_id).await?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn clear_session_persona_view(&self, session_id: &str) -> Result<SessionView> {
        self.clear_session_persona(session_id).await?;
        let agent_id = self.agent_id_for_session(session_id).await?;
        let view = self.session_view(session_id, &agent_id).await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn persist_session_persona_binding(
        &self,
        session_id: &str,
        persona: &PersonaRecord,
    ) -> Result<kheish_types::SessionPersonaBinding> {
        let previous_binding = self.load_session_persona_binding(session_id).await?;
        let default_inline_skills = self
            .resolve_persona_default_skills(
                &persona.persona_id,
                &persona.capability_scope,
                &persona.default_skills,
            )
            .await?;
        if let Some(existing) = previous_binding.as_ref()
            && existing.persona_id == persona.persona_id
            && existing.persona_version == persona.version
            && existing.display_name == persona.display_name
            && existing.soul == persona.soul
            && existing.capability_scope == persona.capability_scope
            && existing.default_inline_skills == default_inline_skills
        {
            return Ok(existing.clone());
        }
        let binding = bind_persona_snapshot(persona, default_inline_skills, now_ms());
        self.session_service
            .save_session_persona_binding(session_id, Some(&binding))
            .await?;
        if let Err(error) = self
            .session_service
            .remember_session_persona(session_id, &binding.persona_id)
            .await
        {
            return match self
                .session_service
                .save_session_persona_binding(session_id, previous_binding.as_ref())
                .await
            {
                Ok(_) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist session persona cache update for {session_id}; metadata rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(binding)
    }

    async fn validate_persona_default_skills(
        &self,
        capability_scope: &kheish_types::CapabilityScope,
        assignments: &[kheish_types::PersonaSkillAssignment],
    ) -> Result<()> {
        self.resolve_persona_default_skills("<validation>", capability_scope, assignments)
            .await
            .map(|_| ())
    }

    async fn resolve_persona_default_skills(
        &self,
        persona_id: &str,
        capability_scope: &kheish_types::CapabilityScope,
        assignments: &[kheish_types::PersonaSkillAssignment],
    ) -> Result<Vec<kheish_types::ActiveSkillSnapshot>> {
        let mut snapshots = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            anyhow::ensure!(
                capability_scope.allows_skill(&assignment.name),
                "persona skill `{}` is excluded by the persona capability scope",
                assignment.name
            );
            let skill = self.skills.get(&assignment.name).ok_or_else(|| {
                anyhow!(
                    "persona default skill `{}` is not installed",
                    assignment.name
                )
            })?;
            skill.validate_inline_activation()?;
            snapshots.push(skill.to_active_snapshot(
                assignment.args.as_deref(),
                kheish_types::SkillExecutionContext::Inline,
                Some(format!("bound by persona {persona_id}")),
            ));
        }
        Ok(snapshots)
    }

    async fn ensure_session_persona_idle(&self, session_id: &str) -> Result<()> {
        if !self
            .session_is_idle_for_topology_mutation(session_id)
            .await?
        {
            bail!(
                "session {session_id} has non-terminal work or live descendants; persona changes are only allowed while the session is idle"
            );
        }
        Ok(())
    }
}
