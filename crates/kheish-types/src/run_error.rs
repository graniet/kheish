//! Run endings with a machine-readable code.
//!
//! Most run failures carry only a message. A few have a cause an embedder
//! must act on without parsing text: the model's answer itself (a refusal,
//! an answer that does not fit the output limit) or a limit the caller set on
//! the run (its turn cap, its wall-clock budget). The engine or the daemon
//! ends such a run with a [`RunTerminalError`], and the daemon copies its code
//! and details onto the run view and its terminal run event.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// Why a model declined a request (`stop_reason: "refusal"` on Anthropic).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRefusal {
    /// The policy area that declined, such as `cyber` or `bio`. Absent when
    /// the refusal maps to no named category, which is a normal value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The provider's human-readable explanation; not stable, display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// The model whose refusal ended the turn (the last of a fallback chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The machine-readable code of a run that ended for a classified reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunErrorCode {
    /// The model declined the request, and no refusal fallback served it.
    /// The run is `failed`.
    ModelRefusal,
    /// The answer was cut off at the output limit, and again when asked once
    /// more with the run's larger retry cap. The run is `failed`.
    OutputLimitExceeded,
    /// The run used every turn its `max_turns` (or the daemon ceiling)
    /// allowed. The run is `failed`.
    MaxTurnsExceeded,
    /// The run outlived its `max_duration_ms`. The run is `cancelled`.
    DeadlineExceeded,
}

impl RunErrorCode {
    /// Returns the stable snake_case code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelRefusal => "model_refusal",
            Self::OutputLimitExceeded => "output_limit_exceeded",
            Self::MaxTurnsExceeded => "max_turns_exceeded",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

/// A classified run ending: a model condition or a run limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTerminalError {
    pub code: RunErrorCode,
    pub message: String,
    /// The refusal, for [`RunErrorCode::ModelRefusal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<ModelRefusal>,
}

impl RunTerminalError {
    /// The model declined the request.
    pub fn refusal(refusal: ModelRefusal) -> Self {
        let category = refusal.category.as_deref().unwrap_or("none");
        let model = refusal
            .model
            .as_deref()
            .map(|model| format!(" by {model}"))
            .unwrap_or_default();
        Self {
            code: RunErrorCode::ModelRefusal,
            message: format!(
                "the model declined the request{model} (refusal category: {category})"
            ),
            refusal: Some(refusal),
        }
    }

    /// The answer did not fit `max_tokens`, then not `retry_max_tokens`.
    pub fn output_limit_exceeded(max_tokens: u32, retry_max_tokens: u32) -> Self {
        Self {
            code: RunErrorCode::OutputLimitExceeded,
            message: format!(
                "the model's answer was cut off at the output limit twice (max_tokens {max_tokens}, then {retry_max_tokens})"
            ),
            refusal: None,
        }
    }

    /// The run used every turn it was allowed.
    pub fn max_turns_exceeded(max_turns: usize) -> Self {
        Self {
            code: RunErrorCode::MaxTurnsExceeded,
            message: format!("agent loop exceeded max_turns={max_turns}"),
            refusal: None,
        }
    }

    /// The run outlived its wall-clock budget.
    pub fn deadline_exceeded(max_duration_ms: u64) -> Self {
        Self {
            code: RunErrorCode::DeadlineExceeded,
            message: format!("run exceeded max_duration_ms={max_duration_ms}"),
            refusal: None,
        }
    }

    /// Finds a terminal error in an error and its sources.
    pub fn find<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a Self> {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(terminal) = error.downcast_ref::<Self>() {
                return Some(terminal);
            }
            current = error.source();
        }
        None
    }
}

impl Display for RunTerminalError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for RunTerminalError {}

#[cfg(test)]
mod tests {
    use super::{ModelRefusal, RunErrorCode, RunTerminalError};

    #[derive(Debug)]
    struct Wrapper(RunTerminalError);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("run failed")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn refusal_carries_its_category_in_the_code_and_message() {
        let error = RunTerminalError::refusal(ModelRefusal {
            category: Some("cyber".to_string()),
            explanation: Some("declined".to_string()),
            model: Some("claude-opus-5".to_string()),
        });
        assert_eq!(error.code, RunErrorCode::ModelRefusal);
        assert_eq!(
            error.to_string(),
            "model_refusal: the model declined the request by claude-opus-5 (refusal category: cyber)"
        );
        assert_eq!(
            serde_json::to_value(error.code).expect("code serializes"),
            "model_refusal"
        );
        let null_category = RunTerminalError::refusal(ModelRefusal::default());
        assert!(null_category.message.ends_with("(refusal category: none)"));
    }

    #[test]
    fn run_limit_endings_have_their_own_codes() {
        let turns = RunTerminalError::max_turns_exceeded(12);
        assert_eq!(turns.code, RunErrorCode::MaxTurnsExceeded);
        assert_eq!(
            turns.to_string(),
            "max_turns_exceeded: agent loop exceeded max_turns=12"
        );
        let deadline = RunTerminalError::deadline_exceeded(600_000);
        assert_eq!(deadline.code.as_str(), "deadline_exceeded");
        assert_eq!(
            serde_json::to_value(deadline.code).expect("code serializes"),
            "deadline_exceeded"
        );
        assert!(deadline.refusal.is_none());
    }

    #[test]
    fn find_walks_the_error_sources() {
        let wrapped = Wrapper(RunTerminalError::output_limit_exceeded(16_000, 32_000));
        let found = RunTerminalError::find(&wrapped).expect("found in the source chain");
        assert_eq!(found.code, RunErrorCode::OutputLimitExceeded);
        assert!(found.message.contains("16000"));
    }
}
