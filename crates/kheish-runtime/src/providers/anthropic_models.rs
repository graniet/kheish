//! Per-generation facts the Anthropic driver needs to shape a Messages API
//! request, keyed by the model family and version that
//! [`kheish_types::parse_claude_model_id`] reads out of any spelling of a model
//! id (first-party, Amazon Bedrock, Google Vertex AI).
//!
//! Source: the Claude API model and migration guides (thinking table, adaptive
//! thinking, effort, preserved thinking, refusals and fallback). One arm per
//! family; extend the table when a generation ships.

use kheish_types::{ClaudeModelFamily, ReasoningConfig, ReasoningEffort, parse_claude_model_id};

/// How a model takes the `thinking` request field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThinkingMode {
    /// Manual extended thinking with `budget_tokens`: Claude Opus 4.6, Claude
    /// Sonnet 4.6 (where adaptive is recommended but the budget still works),
    /// and every earlier generation.
    Budget,
    /// Adaptive thinking only (`budget_tokens` and sampling parameters return a
    /// 400), and the model thinks only when `thinking: {"type": "adaptive"}` is
    /// sent: Claude Opus 4.7 and 4.8.
    AdaptiveOnRequest,
    /// Adaptive thinking only, on whenever `thinking` is omitted: every 5.x
    /// generation. Some reject `{"type": "disabled"}` outright (Claude Opus
    /// 5.5, Claude Sonnet 5.5, the Fable and Mythos models), the others accept
    /// it only at low effort with documented failure modes, so Kheish never
    /// turns thinking off and steers its depth with `output_config.effort`.
    AdaptiveByDefault,
}

/// What the Anthropic driver needs to know about one model generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnthropicModelProfile {
    /// How the model takes thinking.
    pub thinking: ThinkingMode,
    /// The model binds each thinking block to the conversation that produced
    /// it ("preserved thinking"): an edited history invalidates later blocks.
    pub preserved_thinking: bool,
    /// The Messages API accepts the server-side refusal `fallbacks` parameter
    /// for this model.
    pub server_side_fallback: bool,
}

const LEGACY: AnthropicModelProfile = AnthropicModelProfile {
    thinking: ThinkingMode::Budget,
    preserved_thinking: false,
    server_side_fallback: false,
};

const fn profile(
    thinking: ThinkingMode,
    preserved_thinking: bool,
    server_side_fallback: bool,
) -> AnthropicModelProfile {
    AnthropicModelProfile {
        thinking,
        preserved_thinking,
        server_side_fallback,
    }
}

/// Returns the request profile of a model id.
pub(crate) fn anthropic_model_profile(model: &str) -> AnthropicModelProfile {
    use ClaudeModelFamily::{Fable, Haiku, Mythos, Opus, Sonnet};
    use ThinkingMode::{AdaptiveByDefault, AdaptiveOnRequest};
    let Some(id) = parse_claude_model_id(model) else {
        return LEGACY;
    };
    match (id.family, id.major, id.minor) {
        // Claude Fable 5.1 and Claude Mythos 5.1.
        (Fable | Mythos, 5, Some(1)) => profile(AdaptiveByDefault, true, true),
        // Claude Fable 5 takes fallbacks; Claude Mythos 5 runs no classifiers.
        (Fable, 5, None) => profile(AdaptiveByDefault, false, true),
        (Mythos, 5, None) => profile(AdaptiveByDefault, false, false),
        (Opus, 5, Some(5)) => profile(AdaptiveByDefault, true, true),
        (Opus, 5, None) => profile(AdaptiveByDefault, false, true),
        (Sonnet, 5, Some(5)) => profile(AdaptiveByDefault, true, true),
        (Sonnet, 5, None) => profile(AdaptiveByDefault, false, false),
        // Claude Haiku 5.5 has no server-side fallback: a list of fallback
        // models is a 400.
        (Haiku, 5, Some(5)) => profile(AdaptiveByDefault, true, false),
        // A later generation keeps the 5.x request surface: adaptive-only and
        // preserved thinking, as every release since Claude Fable 5.1.
        // Fallbacks stay off until its targets are known.
        (_, major, _) if major >= 5 => profile(AdaptiveByDefault, true, false),
        (Opus, 4, Some(minor)) if minor >= 7 => profile(AdaptiveOnRequest, false, false),
        _ => LEGACY,
    }
}

/// Maps a reasoning effort onto the `output_config.effort` ladder (`low`,
/// `medium`, `high`, `xhigh`, `max`), which every adaptive generation accepts.
/// `none` maps to the lowest rung on models whose thinking cannot be turned
/// off, and to `None` (no thinking) elsewhere.
pub(crate) fn anthropic_effort(
    effort: ReasoningEffort,
    mode: ThinkingMode,
) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => (mode == ThinkingMode::AdaptiveByDefault).then_some("low"),
        ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::Xhigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
    }
}

/// The effort an adaptive request asks for: the explicit effort, or a legacy
/// `budget_tokens` folded onto the nearest rung so a route keeps a comparable
/// depth after a model upgrade. `None` leaves the model's default.
pub(crate) fn anthropic_adaptive_effort(
    reasoning: &ReasoningConfig,
    mode: ThinkingMode,
) -> Option<&'static str> {
    if let Some(effort) = reasoning.effort {
        return anthropic_effort(effort, mode);
    }
    let budget = reasoning.budget_tokens?;
    Some(match budget {
        0..=2_048 => "low",
        2_049..=4_096 => "medium",
        4_097..=8_192 => "high",
        8_193..=32_768 => "xhigh",
        _ => "max",
    })
}

#[cfg(test)]
mod tests {
    use kheish_types::{ReasoningConfig, ReasoningEffort};

    use super::{
        AnthropicModelProfile, ThinkingMode, anthropic_adaptive_effort, anthropic_effort,
        anthropic_model_profile,
    };

    fn expect(model: &str, thinking: ThinkingMode, preserved: bool, fallback: bool) {
        assert_eq!(
            anthropic_model_profile(model),
            AnthropicModelProfile {
                thinking,
                preserved_thinking: preserved,
                server_side_fallback: fallback,
            },
            "{model}"
        );
    }

    #[test]
    fn every_current_family_has_an_explicit_profile() {
        use ThinkingMode::{AdaptiveByDefault, AdaptiveOnRequest, Budget};
        // Adaptive-only, thinking on by default, preserved thinking.
        expect("claude-opus-5-5", AdaptiveByDefault, true, true);
        expect("claude-fable-5-1", AdaptiveByDefault, true, true);
        expect("claude-mythos-5-1", AdaptiveByDefault, true, true);
        expect("claude-sonnet-5-5", AdaptiveByDefault, true, true);
        expect("claude-haiku-5-5", AdaptiveByDefault, true, false);
        // Adaptive-only, thinking on by default, no conversation binding.
        expect("claude-opus-5", AdaptiveByDefault, false, true);
        expect("claude-fable-5", AdaptiveByDefault, false, true);
        expect("claude-mythos-5", AdaptiveByDefault, false, false);
        expect("claude-sonnet-5", AdaptiveByDefault, false, false);
        // Adaptive-only, thinking only when asked.
        expect("claude-opus-4-8", AdaptiveOnRequest, false, false);
        expect("claude-opus-4-7", AdaptiveOnRequest, false, false);
        // budget_tokens still accepted.
        expect("claude-opus-4-6", Budget, false, false);
        expect("claude-sonnet-4-6", Budget, false, false);
        expect("claude-opus-4-5", Budget, false, false);
        expect("claude-sonnet-4-5-20250929", Budget, false, false);
        expect("claude-haiku-4-5-20251001", Budget, false, false);
        expect("claude-3-5-sonnet-20241022", Budget, false, false);
        expect("claude-test", Budget, false, false);
    }

    #[test]
    fn cloud_platform_spellings_resolve_to_the_same_profile() {
        use ThinkingMode::{AdaptiveByDefault, AdaptiveOnRequest, Budget};
        expect("anthropic.claude-opus-5-5", AdaptiveByDefault, true, true);
        expect(
            "us.anthropic.claude-sonnet-5-5-v1:0",
            AdaptiveByDefault,
            true,
            true,
        );
        expect(
            "eu.anthropic.claude-haiku-5-5",
            AdaptiveByDefault,
            true,
            false,
        );
        expect("anthropic.claude-opus-4-8", AdaptiveOnRequest, false, false);
        expect(
            "anthropic.claude-opus-4-7-v1:0",
            AdaptiveOnRequest,
            false,
            false,
        );
        expect("claude-opus-4-5@20251101", Budget, false, false);
        expect("claude-opus-4-6[1m]", Budget, false, false);
    }

    #[test]
    fn a_later_generation_keeps_the_5x_request_surface() {
        expect(
            "claude-opus-5-6",
            ThinkingMode::AdaptiveByDefault,
            true,
            false,
        );
        expect(
            "claude-sonnet-6",
            ThinkingMode::AdaptiveByDefault,
            true,
            false,
        );
    }

    #[test]
    fn effort_maps_onto_the_five_rung_ladder() {
        use ReasoningEffort::*;
        let by_default = ThinkingMode::AdaptiveByDefault;
        let on_request = ThinkingMode::AdaptiveOnRequest;
        assert_eq!(anthropic_effort(None, by_default), Some("low"));
        assert_eq!(anthropic_effort(None, on_request), Option::None);
        assert_eq!(anthropic_effort(Minimal, by_default), Some("low"));
        assert_eq!(anthropic_effort(Low, by_default), Some("low"));
        assert_eq!(anthropic_effort(Medium, by_default), Some("medium"));
        assert_eq!(anthropic_effort(High, on_request), Some("high"));
        assert_eq!(anthropic_effort(Xhigh, by_default), Some("xhigh"));
        assert_eq!(anthropic_effort(Max, by_default), Some("max"));
    }

    #[test]
    fn a_legacy_budget_folds_onto_the_nearest_rung() {
        let budget = |budget_tokens| ReasoningConfig {
            budget_tokens: Some(budget_tokens),
            ..ReasoningConfig::default()
        };
        let mode = ThinkingMode::AdaptiveByDefault;
        assert_eq!(anthropic_adaptive_effort(&budget(1_024), mode), Some("low"));
        assert_eq!(
            anthropic_adaptive_effort(&budget(4_096), mode),
            Some("medium")
        );
        assert_eq!(
            anthropic_adaptive_effort(&budget(8_192), mode),
            Some("high")
        );
        assert_eq!(
            anthropic_adaptive_effort(&budget(16_000), mode),
            Some("xhigh")
        );
        assert_eq!(
            anthropic_adaptive_effort(&budget(64_000), mode),
            Some("max")
        );
        assert_eq!(
            anthropic_adaptive_effort(&ReasoningConfig::default(), mode),
            None
        );
    }
}
