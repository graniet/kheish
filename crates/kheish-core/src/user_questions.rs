//! Shared validation and rendering helpers for structured user-question flows.

use std::collections::BTreeSet;
use std::fmt;

use anyhow::Result;
use kheish_types::{UserQuestionRequest, UserQuestionResolution};
use serde_json::{Value, json};

/// Stable validation codes for structured user-question resolutions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserQuestionValidationCode {
    RequestMismatch,
    OptionNotFound,
    AnswerMissing,
    DuplicateAnswer,
    DuplicateOption,
    DeclinedWithAnswers,
    SingleSelectViolation,
    AnswerEmpty,
    UnknownAnswer,
}

impl UserQuestionValidationCode {
    pub fn problem_code(self) -> &'static str {
        match self {
            Self::RequestMismatch => "question_request_mismatch",
            Self::OptionNotFound => "question_option_not_found",
            Self::AnswerMissing => "question_answer_missing",
            Self::DuplicateAnswer => "question_duplicate_answer",
            Self::DuplicateOption => "question_duplicate_option",
            Self::DeclinedWithAnswers => "question_declined_with_answers",
            Self::SingleSelectViolation => "question_single_select_violation",
            Self::AnswerEmpty => "question_answer_empty",
            Self::UnknownAnswer => "question_unknown_answer",
        }
    }
}

/// Typed error returned when a structured user-question resolution is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestionValidationError {
    code: UserQuestionValidationCode,
    detail: String,
}

impl UserQuestionValidationError {
    fn new(code: UserQuestionValidationCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn code(&self) -> UserQuestionValidationCode {
        self.code
    }

    pub fn problem_code(&self) -> &'static str {
        self.code.problem_code()
    }
}

impl fmt::Display for UserQuestionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.detail.fmt(formatter)
    }
}

impl std::error::Error for UserQuestionValidationError {}

/// Renders one validated structured user-question resolution into a canonical JSON payload.
pub fn render_user_question_resolution(
    request: &UserQuestionRequest,
    resolution: &UserQuestionResolution,
) -> Result<Value> {
    if resolution.request_id != request.id {
        return Err(UserQuestionValidationError::new(
            UserQuestionValidationCode::RequestMismatch,
            format!(
                "user-question resolution {} does not match pending request {}",
                resolution.request_id, request.id
            ),
        )
        .into());
    }

    if resolution.declined {
        if !resolution.answers.is_empty() {
            return Err(UserQuestionValidationError::new(
                UserQuestionValidationCode::DeclinedWithAnswers,
                "declined user-question resolutions must not include answers",
            )
            .into());
        }
        return Ok(json!({
            "request_id": request.id,
            "declined": true,
            "justification": resolution.justification,
        }));
    }

    let mut answers_by_question = std::collections::BTreeMap::new();
    for answer in &resolution.answers {
        if answers_by_question
            .insert(answer.question_id.clone(), answer.clone())
            .is_some()
        {
            return Err(UserQuestionValidationError::new(
                UserQuestionValidationCode::DuplicateAnswer,
                format!("duplicate answer for question {}", answer.question_id),
            )
            .into());
        }
    }

    let mut rendered_answers = Vec::with_capacity(request.questions.len());
    for question in &request.questions {
        let answer = answers_by_question.remove(&question.id).ok_or_else(|| {
            UserQuestionValidationError::new(
                UserQuestionValidationCode::AnswerMissing,
                format!("missing answer for question {}", question.id),
            )
        })?;
        if !question.multi_select && answer.selected_option_ids.len() > 1 {
            return Err(UserQuestionValidationError::new(
                UserQuestionValidationCode::SingleSelectViolation,
                format!("question {} allows only one option", question.id),
            )
            .into());
        }
        let mut selected_options = Vec::new();
        let mut selected_option_ids = BTreeSet::new();
        for option_id in &answer.selected_option_ids {
            if !selected_option_ids.insert(option_id.as_str()) {
                return Err(UserQuestionValidationError::new(
                    UserQuestionValidationCode::DuplicateOption,
                    format!(
                        "duplicate option {} for question {}",
                        option_id, question.id
                    ),
                )
                .into());
            }
            let option = question
                .options
                .iter()
                .find(|candidate| candidate.id == *option_id)
                .ok_or_else(|| {
                    UserQuestionValidationError::new(
                        UserQuestionValidationCode::OptionNotFound,
                        format!("unknown option {} for question {}", option_id, question.id),
                    )
                })?;
            selected_options.push(json!({
                "id": option.id,
                "label": option.label,
                "description": option.description,
            }));
        }
        if selected_options.is_empty()
            && answer
                .freeform_answer
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            return Err(UserQuestionValidationError::new(
                UserQuestionValidationCode::AnswerEmpty,
                format!("question {} requires at least one answer", question.id),
            )
            .into());
        }
        rendered_answers.push(json!({
            "question_id": question.id,
            "header": question.header,
            "question": question.question,
            "selected_option_ids": answer.selected_option_ids,
            "selected_options": selected_options,
            "freeform_answer": answer.freeform_answer,
        }));
    }
    if !answers_by_question.is_empty() {
        return Err(UserQuestionValidationError::new(
            UserQuestionValidationCode::UnknownAnswer,
            "resolution contains answers for unknown questions",
        )
        .into());
    }

    let summary = rendered_answers
        .iter()
        .map(|answer| {
            let question = answer["question"].as_str().unwrap_or_default();
            let selected = answer["selected_options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| option["label"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let freeform = answer["freeform_answer"].as_str().unwrap_or_default();
            if !selected.is_empty() {
                format!("{question}: {selected}")
            } else {
                format!("{question}: {freeform}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(json!({
        "request_id": request.id,
        "declined": false,
        "summary": summary,
        "answers": rendered_answers,
        "justification": resolution.justification,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kheish_types::{UserQuestion, UserQuestionAnswer, UserQuestionOption};

    fn request(multi_select: bool) -> UserQuestionRequest {
        UserQuestionRequest {
            id: "request-1".to_string(),
            tool_call_id: "call-1".to_string(),
            questions: vec![UserQuestion {
                id: "focus".to_string(),
                header: "Focus".to_string(),
                question: "Which focus?".to_string(),
                options: vec![
                    UserQuestionOption {
                        id: "memory".to_string(),
                        label: "Memory".to_string(),
                        description: None,
                        preview: None,
                    },
                    UserQuestionOption {
                        id: "kernel".to_string(),
                        label: "Kernel".to_string(),
                        description: None,
                        preview: None,
                    },
                ],
                multi_select,
            }],
            created_at_ms: 1,
            expires_at_ms: None,
        }
    }

    #[test]
    fn rejects_duplicate_selected_option_ids() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![UserQuestionAnswer {
                    question_id: "focus".to_string(),
                    selected_option_ids: vec!["memory".to_string(), "memory".to_string()],
                    freeform_answer: None,
                }],
                declined: false,
                justification: None,
            },
        )
        .expect_err("duplicate option ids should fail");

        assert_eq!(
            error.to_string(),
            "duplicate option memory for question focus"
        );
    }

    #[test]
    fn rejects_duplicate_answers_for_same_question() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![
                    UserQuestionAnswer {
                        question_id: "focus".to_string(),
                        selected_option_ids: vec!["memory".to_string()],
                        freeform_answer: None,
                    },
                    UserQuestionAnswer {
                        question_id: "focus".to_string(),
                        selected_option_ids: vec!["kernel".to_string()],
                        freeform_answer: None,
                    },
                ],
                declined: false,
                justification: None,
            },
        )
        .expect_err("duplicate question answers should fail");

        assert_eq!(error.to_string(), "duplicate answer for question focus");
    }

    #[test]
    fn rejects_declined_resolution_with_answers() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![UserQuestionAnswer {
                    question_id: "focus".to_string(),
                    selected_option_ids: vec!["memory".to_string()],
                    freeform_answer: None,
                }],
                declined: true,
                justification: None,
            },
        )
        .expect_err("declined resolutions must not include answers");

        assert_eq!(
            error.to_string(),
            "declined user-question resolutions must not include answers"
        );
    }

    #[test]
    fn rejects_missing_required_question_answer() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: Vec::new(),
                declined: false,
                justification: None,
            },
        )
        .expect_err("missing answers should fail");

        assert_eq!(error.to_string(), "missing answer for question focus");
    }

    #[test]
    fn rejects_unknown_question_answers() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![
                    UserQuestionAnswer {
                        question_id: "focus".to_string(),
                        selected_option_ids: vec!["memory".to_string()],
                        freeform_answer: None,
                    },
                    UserQuestionAnswer {
                        question_id: "other".to_string(),
                        selected_option_ids: Vec::new(),
                        freeform_answer: Some("extra".to_string()),
                    },
                ],
                declined: false,
                justification: None,
            },
        )
        .expect_err("answers for unknown questions should fail");

        assert_eq!(
            error.to_string(),
            "resolution contains answers for unknown questions"
        );
    }

    #[test]
    fn rejects_unknown_option_id() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![UserQuestionAnswer {
                    question_id: "focus".to_string(),
                    selected_option_ids: vec!["latency".to_string()],
                    freeform_answer: None,
                }],
                declined: false,
                justification: None,
            },
        )
        .expect_err("unknown options should fail");

        assert_eq!(
            error.to_string(),
            "unknown option latency for question focus"
        );
    }

    #[test]
    fn rejects_multiple_options_for_single_select() {
        let error = render_user_question_resolution(
            &request(false),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![UserQuestionAnswer {
                    question_id: "focus".to_string(),
                    selected_option_ids: vec!["memory".to_string(), "kernel".to_string()],
                    freeform_answer: None,
                }],
                declined: false,
                justification: None,
            },
        )
        .expect_err("single-select questions should reject multiple options");

        assert_eq!(error.to_string(), "question focus allows only one option");
    }

    #[test]
    fn rejects_empty_answer_without_selection_or_freeform() {
        let error = render_user_question_resolution(
            &request(true),
            &UserQuestionResolution {
                request_id: "request-1".to_string(),
                answers: vec![UserQuestionAnswer {
                    question_id: "focus".to_string(),
                    selected_option_ids: Vec::new(),
                    freeform_answer: Some("   ".to_string()),
                }],
                declined: false,
                justification: None,
            },
        )
        .expect_err("empty answers should fail");

        assert_eq!(
            error.to_string(),
            "question focus requires at least one answer"
        );
    }
}
