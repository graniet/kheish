//! Model-backed semantic extraction helpers for daemon-owned learning capture.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use kheish_types::{LearningKind, StructuredFieldSchema, StructuredValueKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hooks::DaemonHookDispatcher;
use crate::learning::{
    LearningSemanticCaptureConfig, learning_content_has_secret_material,
    semantic_learning_content_key,
};
use crate::{DaemonRunStatus, RunMemoryRecord, RunRecord};

const DEFAULT_SEMANTIC_CAPTURE_TIMEOUT_MS: u64 = 15_000;
const MAX_SEMANTIC_CAPTURE_REASONABLE_CONFIDENCE: u8 = 100;

/// One normalized daemon-owned semantic candidate draft extracted from a completed run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LearningSemanticCandidateDraft {
    pub(crate) kind: LearningKind,
    pub(crate) content: String,
    pub(crate) confidence: u8,
}

/// Executes model-backed semantic extraction for daemon-owned learning capture.
#[derive(Clone)]
pub(crate) struct LearningExtractionService {
    hooks: Arc<DaemonHookDispatcher>,
}

impl LearningExtractionService {
    /// Creates a new extraction service bound to the daemon hook/model runtime.
    pub(crate) fn new(hooks: Arc<DaemonHookDispatcher>) -> Self {
        Self { hooks }
    }

    /// Extracts conservative semantic-memory drafts from one completed run.
    pub(crate) async fn extract_semantic_candidates(
        &self,
        record: &RunRecord,
        memory_record: &RunMemoryRecord,
        settings: &LearningSemanticCaptureConfig,
    ) -> Result<Vec<LearningSemanticCandidateDraft>> {
        if !settings.enabled || record.view.status != DaemonRunStatus::Completed {
            return Ok(Vec::new());
        }
        let heuristic = heuristic_candidates(memory_record, settings.max_candidates_per_run);
        if !heuristic.is_empty() {
            return Ok(heuristic);
        }
        let prompt =
            build_extraction_prompt(record, memory_record, settings.max_candidates_per_run);
        let payload = self
            .hooks
            .run_structured_prompt_json(
                Some(&record.view.session_id),
                Some(&record.view.run_id),
                &prompt,
                Some(LEARNING_EXTRACTION_SYSTEM_PROMPT),
                settings.model.as_ref(),
                settings
                    .timeout_ms
                    .or(Some(DEFAULT_SEMANTIC_CAPTURE_TIMEOUT_MS)),
                learning_extraction_schema(settings.max_candidates_per_run),
            )
            .await?;
        Ok(normalize_candidates(
            parse_wire_candidates(payload, settings.max_candidates_per_run)?,
            settings.max_candidates_per_run,
        ))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LearningExtractionCandidateWire {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    confidence: Option<u64>,
}

fn build_extraction_prompt(
    record: &RunRecord,
    memory_record: &RunMemoryRecord,
    max_candidates_per_run: usize,
) -> String {
    let payload = json!({
        "run": {
            "run_id": record.view.run_id,
            "session_id": record.view.session_id,
            "agent_id": record.view.agent_id,
            "kind": record.view.kind,
            "status": record.view.status,
            "request_preview": memory_record.memory.request_preview,
            "outcome_preview": memory_record.memory.outcome_preview,
            "summary": memory_record.memory.summary,
            "failure_markers": memory_record.memory.failure_markers,
        },
        "allowed_kinds": ["fact", "preference", "decision"],
        "max_candidates_per_run": max_candidates_per_run,
        "instructions": [
            "Extract only conservative durable memory that should help future turns in the same session.",
            "Prefer explicit user-stated facts, preferences, or stable decisions made during the run.",
            "When the request explicitly says remember, preferred, preference, always, or codename, treat that as a strong extraction signal.",
            "Prefer the user's explicit statements over the assistant's reply wording.",
            "Do not extract one-off task instructions, ephemeral outputs, summaries, procedures, secrets, or speculative inferences.",
            "When nothing should be remembered, return an empty JSON object.",
            "Return only JSON matching the requested schema."
        ]
    });
    serde_json::to_string_pretty(&payload)
        .expect("learning extraction prompt serialization must work")
}

fn normalize_candidates(
    candidates: Vec<LearningExtractionCandidateWire>,
    max_candidates_per_run: usize,
) -> Vec<LearningSemanticCandidateDraft> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for candidate in candidates {
        let Some(kind) = normalize_kind(&candidate.kind) else {
            continue;
        };
        let content = normalize_content(&candidate.content);
        if content.is_empty() {
            continue;
        }
        if learning_content_has_secret_material(&content) {
            continue;
        }
        let key = format!("{kind:?}:{}", semantic_learning_content_key(&content));
        if !seen.insert(key) {
            continue;
        }
        normalized.push(LearningSemanticCandidateDraft {
            kind,
            content,
            confidence: candidate
                .confidence
                .unwrap_or(80)
                .min(MAX_SEMANTIC_CAPTURE_REASONABLE_CONFIDENCE as u64)
                as u8,
        });
        if normalized.len() >= max_candidates_per_run {
            break;
        }
    }
    normalized
}

fn heuristic_candidates(
    memory_record: &RunMemoryRecord,
    max_candidates_per_run: usize,
) -> Vec<LearningSemanticCandidateDraft> {
    let Some(request) = memory_record.memory.request_preview.as_deref() else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    let lowered = request.to_ascii_lowercase();
    for (label, kind) in [
        ("preference:", LearningKind::Preference),
        ("fact:", LearningKind::Fact),
        ("decision:", LearningKind::Decision),
    ] {
        let mut offset = 0usize;
        while let Some(relative) = lowered[offset..].find(label) {
            let start = offset + relative;
            matches.push((start, start + label.len(), kind.clone()));
            offset = start + label.len();
        }
    }
    matches.sort_by_key(|(start, _, _)| *start);
    if matches.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for (index, (_, content_start, kind)) in matches.iter().enumerate() {
        let end = matches
            .get(index + 1)
            .map(|(next_start, _, _)| *next_start)
            .unwrap_or(request.len());
        let content = truncate_heuristic_content(&request[*content_start..end]);
        if content.is_empty() {
            continue;
        }
        if learning_content_has_secret_material(&content) {
            continue;
        }
        candidates.push(LearningSemanticCandidateDraft {
            kind: kind.clone(),
            content,
            confidence: 98,
        });
        if candidates.len() >= max_candidates_per_run {
            break;
        }
    }
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            seen.insert(format!(
                "{:?}:{}",
                candidate.kind,
                semantic_learning_content_key(&candidate.content)
            ))
        })
        .collect()
}

fn truncate_heuristic_content(value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    let mut end = value.len();
    for marker in [" reply exactly ", " respond exactly ", " answer exactly "] {
        if let Some(index) = lowered.find(marker) {
            end = end.min(index);
        }
    }
    let trimmed = value[..end]
        .trim()
        .trim_matches(|ch: char| ch == '.' || ch == ';' || ch == ',' || ch.is_whitespace());
    normalize_content(trimmed)
}

fn parse_wire_candidates(
    payload: Value,
    max_candidates_per_run: usize,
) -> Result<Vec<LearningExtractionCandidateWire>> {
    let mut candidates = Vec::new();
    let Some(object) = payload.as_object() else {
        return Ok(candidates);
    };
    for index in 1..=max_candidates_per_run {
        let key = format!("candidate_{index}");
        let Some(value) = object.get(&key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        candidates.push(serde_json::from_value(value.clone())?);
    }
    Ok(candidates)
}

fn normalize_kind(value: &str) -> Option<LearningKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fact" => Some(LearningKind::Fact),
        "preference" => Some(LearningKind::Preference),
        "decision" => Some(LearningKind::Decision),
        _ => None,
    }
}

fn normalize_content(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn learning_extraction_schema(max_candidates_per_run: usize) -> StructuredFieldSchema {
    let mut item = StructuredFieldSchema::new(StructuredValueKind::Object);
    item.fields.insert(
        "kind".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    item.fields.insert(
        "content".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::String),
    );
    item.optional_fields.insert(
        "confidence".to_string(),
        StructuredFieldSchema::new(StructuredValueKind::Number),
    );
    let mut root = StructuredFieldSchema::new(StructuredValueKind::Object);
    for index in 1..=max_candidates_per_run {
        root.optional_fields
            .insert(format!("candidate_{index}"), item.clone());
    }
    root
}

const LEARNING_EXTRACTION_SYSTEM_PROMPT: &str = r#"You are Kheish's daemon-owned semantic memory extractor.

Review one completed run and propose a few conservative durable-memory candidates.
Allowed kinds:
- fact
- preference
- decision

Prefer explicit user-stated information or stable decisions.
Explicit remember/preference/codename statements are strong extraction signals.
Do not extract summaries, procedures, ephemeral task instructions, or speculative inferences.
When uncertain, return fewer candidates.
Return only one JSON object with optional fields `candidate_1`, `candidate_2`, and so on."#;

#[cfg(test)]
mod tests {
    use super::{
        LearningExtractionCandidateWire, LearningSemanticCandidateDraft, heuristic_candidates,
        normalize_candidates,
    };
    use crate::RunMemoryRecord;
    use kheish_types::LearningKind;

    #[test]
    fn normalize_candidates_filters_unknown_kinds_and_duplicates() {
        let normalized = normalize_candidates(
            vec![
                LearningExtractionCandidateWire {
                    kind: "preference".to_string(),
                    content: "  Preferred   editor   is   Helix. ".to_string(),
                    confidence: Some(101),
                },
                LearningExtractionCandidateWire {
                    kind: "preference".to_string(),
                    content: "preferred editor: helix".to_string(),
                    confidence: Some(80),
                },
                LearningExtractionCandidateWire {
                    kind: "procedure".to_string(),
                    content: "Never keep this.".to_string(),
                    confidence: Some(90),
                },
                LearningExtractionCandidateWire {
                    kind: "decision".to_string(),
                    content: String::new(),
                    confidence: Some(90),
                },
            ],
            4,
        );

        assert_eq!(
            normalized,
            vec![LearningSemanticCandidateDraft {
                kind: LearningKind::Preference,
                content: "Preferred editor is Helix.".to_string(),
                confidence: 100,
            }]
        );
    }

    #[test]
    fn normalize_candidates_respects_max_candidates() {
        let normalized = normalize_candidates(
            vec![
                LearningExtractionCandidateWire {
                    kind: "fact".to_string(),
                    content: "one".to_string(),
                    confidence: Some(70),
                },
                LearningExtractionCandidateWire {
                    kind: "preference".to_string(),
                    content: "two".to_string(),
                    confidence: Some(80),
                },
                LearningExtractionCandidateWire {
                    kind: "decision".to_string(),
                    content: "three".to_string(),
                    confidence: Some(90),
                },
            ],
            2,
        );

        assert_eq!(
            normalized,
            vec![
                LearningSemanticCandidateDraft {
                    kind: LearningKind::Fact,
                    content: "one".to_string(),
                    confidence: 70,
                },
                LearningSemanticCandidateDraft {
                    kind: LearningKind::Preference,
                    content: "two".to_string(),
                    confidence: 80,
                },
            ]
        );
    }

    #[test]
    fn normalize_candidates_rejects_secret_like_content() {
        let normalized = normalize_candidates(
            vec![
                LearningExtractionCandidateWire {
                    kind: "fact".to_string(),
                    content: "The API key is sk-proj-secret.".to_string(),
                    confidence: Some(99),
                },
                LearningExtractionCandidateWire {
                    kind: "fact".to_string(),
                    content: "Access token is <redacted>.".to_string(),
                    confidence: Some(99),
                },
                LearningExtractionCandidateWire {
                    kind: "preference".to_string(),
                    content: "Preferred editor is Helix.".to_string(),
                    confidence: Some(92),
                },
            ],
            4,
        );

        assert_eq!(
            normalized,
            vec![LearningSemanticCandidateDraft {
                kind: LearningKind::Preference,
                content: "Preferred editor is Helix.".to_string(),
                confidence: 92,
            }]
        );
    }

    #[test]
    fn heuristic_candidates_extract_explicit_labeled_memory_items() {
        let memory_record = RunMemoryRecord {
            session_id: "demo".to_string(),
            scope_keys: vec!["session:demo".to_string()],
            semantic_capture: crate::memory::RunMemorySemanticCaptureState::Pending,
            memory: kheish_types::RecoveredMemoryEntry {
                run_id: "run-1".to_string(),
                recorded_at_ms: 1,
                status: "completed".to_string(),
                request_preview: Some("For future turns, retain these durable memory items exactly: Preference: Preferred editor is Helix. Fact: Project codename is Atlas. Reply exactly OK.".to_string()),
                outcome_preview: Some("OK".to_string()),
                artifact_ids: Vec::new(),
                failure_markers: Vec::new(),
                summary: "Request: ...".to_string(),
            },
        };

        assert_eq!(
            heuristic_candidates(&memory_record, 4),
            vec![
                LearningSemanticCandidateDraft {
                    kind: LearningKind::Preference,
                    content: "Preferred editor is Helix".to_string(),
                    confidence: 98,
                },
                LearningSemanticCandidateDraft {
                    kind: LearningKind::Fact,
                    content: "Project codename is Atlas".to_string(),
                    confidence: 98,
                },
            ]
        );
    }

    #[test]
    fn heuristic_candidates_reject_secret_like_items() {
        let memory_record = RunMemoryRecord {
            session_id: "demo".to_string(),
            scope_keys: vec!["session:demo".to_string()],
            semantic_capture: crate::memory::RunMemorySemanticCaptureState::Pending,
            memory: kheish_types::RecoveredMemoryEntry {
                run_id: "run-1".to_string(),
                recorded_at_ms: 1,
                status: "completed".to_string(),
                request_preview: Some("For future turns, retain these durable memory items exactly: Fact: Access token is <redacted>. Fact: Project codename is Atlas. Reply exactly OK.".to_string()),
                outcome_preview: Some("OK".to_string()),
                artifact_ids: Vec::new(),
                failure_markers: Vec::new(),
                summary: "Request: ...".to_string(),
            },
        };

        assert_eq!(
            heuristic_candidates(&memory_record, 4),
            vec![LearningSemanticCandidateDraft {
                kind: LearningKind::Fact,
                content: "Project codename is Atlas".to_string(),
                confidence: 98,
            }]
        );
    }
}
