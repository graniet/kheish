//! Per-generation facts the Anthropic driver needs to shape a Messages API
//! request, keyed by the model family and version that
//! [`kheish_types::parse_claude_model_id`] reads out of any spelling of a model
//! id (first-party, Amazon Bedrock, Google Vertex AI).
//!
//! Source: the Claude API model and migration guides (thinking table, adaptive
//! thinking, effort, preserved thinking, refusals and fallback). One arm per
//! family; extend the table when a generation ships.

use kheish_types::{
    ClaudeModelFamily, ModelUsage, ReasoningConfig, ReasoningEffort, parse_claude_model_id,
};

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

/// Returns whether the model takes `fallbacks` as a list of models. Claude
/// Sonnet 5.5 takes only `"default"`.
pub(crate) fn anthropic_accepts_fallback_models(model: &str) -> bool {
    anthropic_model_profile(model).server_side_fallback
        && !parse_claude_model_id(model)
            .is_some_and(|id| id.is(ClaudeModelFamily::Sonnet, 5, Some(5)))
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

/// Token prices of one model, in US dollars per million tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct AnthropicPricing {
    /// Prompt tokens billed at the full input rate.
    pub input_per_million_tokens_usd: f64,
    /// Output tokens, thinking included.
    pub output_per_million_tokens_usd: f64,
    /// Prompt tokens written to the 5-minute prompt cache.
    pub cache_write_per_million_tokens_usd: f64,
    /// Prompt tokens written to the 1-hour prompt cache.
    pub cache_write_1h_per_million_tokens_usd: f64,
    /// Prompt tokens read from the prompt cache (hits and refreshes).
    pub cache_read_per_million_tokens_usd: f64,
}

impl AnthropicPricing {
    /// Prices with the standard prompt-cache multipliers: 5-minute writes at
    /// 1.25 times the input price, 1-hour writes at twice it, reads at 0.1
    /// times it.
    pub fn with_standard_cache_rates(input: f64, output: f64) -> Self {
        Self::with_cache_read(input, output, input * 0.1)
    }

    fn with_cache_read(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input_per_million_tokens_usd: input,
            output_per_million_tokens_usd: output,
            cache_write_per_million_tokens_usd: input * 1.25,
            cache_write_1h_per_million_tokens_usd: input * 2.0,
            cache_read_per_million_tokens_usd: cache_read,
        }
    }

    /// Prices one usage report. `cache_write_1h_tokens` is the part of
    /// `cache_creation_input_tokens` written to the 1-hour cache.
    pub(crate) fn cost_usd(&self, usage: &ModelUsage, cache_write_1h_tokens: u64) -> f64 {
        let per_million = |tokens: u64, price: f64| tokens as f64 / 1_000_000.0 * price;
        let cache_write_1h = cache_write_1h_tokens.min(usage.cache_creation_input_tokens);
        let cache_write_5m = usage.cache_creation_input_tokens - cache_write_1h;
        per_million(usage.input_tokens, self.input_per_million_tokens_usd)
            + per_million(usage.output_tokens, self.output_per_million_tokens_usd)
            + per_million(cache_write_5m, self.cache_write_per_million_tokens_usd)
            + per_million(cache_write_1h, self.cache_write_1h_per_million_tokens_usd)
            + per_million(
                usage.cache_read_input_tokens,
                self.cache_read_per_million_tokens_usd,
            )
    }
}

/// Claude Haiku 5.5 bills a request at its higher prices when the prompt,
/// cache reads and writes included, is over this many tokens.
const HAIKU_5_5_LONG_PROMPT_TOKENS: u64 = 100_000;

/// Returns a model's first-party list price, or `None` for a model without a
/// published price. `prompt_tokens` (cache reads and writes included) picks
/// Claude Haiku 5.5's price tier.
///
/// Source: <https://platform.claude.com/docs/en/about-claude/pricing>, read
/// 2026-11-07. Cache reads are 0.1 times the input price except on Claude
/// Fable 5.1 and Claude Mythos 5.1 (0.025 times) and Claude Opus 5.5 and
/// Claude Sonnet 5.5 (0.05 times).
pub(crate) fn anthropic_list_pricing(model: &str, prompt_tokens: u64) -> Option<AnthropicPricing> {
    use ClaudeModelFamily::{Fable, Haiku, Mythos, Opus, Sonnet};
    let id = parse_claude_model_id(model)?;
    let standard = AnthropicPricing::with_standard_cache_rates;
    let pricing = match (id.family, id.major, id.minor) {
        (Fable | Mythos, 5, Some(1)) => AnthropicPricing::with_cache_read(10.0, 50.0, 0.25),
        (Fable | Mythos, 5, None) => standard(10.0, 50.0),
        (Opus, 5, Some(5)) => AnthropicPricing::with_cache_read(4.0, 20.0, 0.20),
        (Opus, 5, None) => standard(5.0, 25.0),
        (Opus, 4, Some(5..=8)) => standard(5.0, 25.0),
        (Opus, 4, None | Some(1)) => standard(15.0, 75.0),
        (Sonnet, 5, Some(5)) => AnthropicPricing::with_cache_read(2.0, 10.0, 0.10),
        (Sonnet, 5, None) => standard(2.0, 10.0),
        (Sonnet, 4, None | Some(5) | Some(6)) => standard(3.0, 15.0),
        (Haiku, 5, Some(5)) if prompt_tokens > HAIKU_5_5_LONG_PROMPT_TOKENS => standard(0.50, 2.50),
        (Haiku, 5, Some(5)) => standard(0.10, 0.50),
        (Haiku, 4, Some(5)) => standard(1.0, 5.0),
        _ => return None,
    };
    Some(pricing)
}

#[cfg(test)]
mod tests {
    use kheish_types::{ModelUsage, ReasoningConfig, ReasoningEffort};

    use super::{
        AnthropicModelProfile, AnthropicPricing, ThinkingMode, anthropic_adaptive_effort,
        anthropic_effort, anthropic_list_pricing, anthropic_model_profile,
    };

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn list_prices_match_the_published_table() {
        let check = |model: &str, prompt: u64, rates: [f64; 5]| {
            let pricing = anthropic_list_pricing(model, prompt).expect(model);
            let actual = [
                pricing.input_per_million_tokens_usd,
                pricing.output_per_million_tokens_usd,
                pricing.cache_write_per_million_tokens_usd,
                pricing.cache_write_1h_per_million_tokens_usd,
                pricing.cache_read_per_million_tokens_usd,
            ];
            for (actual, expected) in actual.iter().zip(rates) {
                assert!(close(*actual, expected), "{model}: {actual:?} != {rates:?}");
            }
        };
        // input, output, 5m cache write, 1h cache write, cache read
        check("claude-fable-5-1", 0, [10.0, 50.0, 12.5, 20.0, 0.25]);
        check("claude-mythos-5-1", 0, [10.0, 50.0, 12.5, 20.0, 0.25]);
        check("claude-fable-5", 0, [10.0, 50.0, 12.5, 20.0, 1.0]);
        check("claude-opus-5-5", 0, [4.0, 20.0, 5.0, 8.0, 0.20]);
        check("anthropic.claude-opus-5-5", 0, [4.0, 20.0, 5.0, 8.0, 0.20]);
        check("claude-opus-5", 0, [5.0, 25.0, 6.25, 10.0, 0.50]);
        check("claude-opus-4-8", 0, [5.0, 25.0, 6.25, 10.0, 0.50]);
        check("claude-opus-4-5", 0, [5.0, 25.0, 6.25, 10.0, 0.50]);
        check("claude-opus-4-1", 0, [15.0, 75.0, 18.75, 30.0, 1.50]);
        check("claude-sonnet-5-5", 0, [2.0, 10.0, 2.5, 4.0, 0.10]);
        check("claude-sonnet-5", 0, [2.0, 10.0, 2.5, 4.0, 0.20]);
        check("claude-sonnet-4-6", 0, [3.0, 15.0, 3.75, 6.0, 0.30]);
        check("claude-haiku-5-5", 100_000, [0.10, 0.50, 0.125, 0.20, 0.01]);
        check("claude-haiku-5-5", 100_001, [0.50, 2.50, 0.625, 1.0, 0.05]);
        check("claude-haiku-4-5-20251001", 0, [1.0, 5.0, 1.25, 2.0, 0.10]);
        assert!(anthropic_list_pricing("claude-test", 0).is_none());
    }

    #[test]
    fn cost_prices_each_token_count_at_its_own_rate() {
        let pricing = anthropic_list_pricing("claude-opus-5-5", 0).expect("priced");
        let usage = ModelUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 3_000_000,
            cache_read_input_tokens: 10_000_000,
            ..ModelUsage::default()
        };
        // 4 + 20 + 2M x 5 + 1M x 8 + 10M x 0.20
        assert!(close(
            pricing.cost_usd(&usage, 1_000_000),
            4.0 + 20.0 + 10.0 + 8.0 + 2.0
        ));
        let flat = AnthropicPricing::with_standard_cache_rates(1.0, 2.0);
        assert!(close(flat.cost_usd(&usage, 0), 1.0 + 2.0 + 3.75 + 1.0));
    }

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
    fn sonnet_5_5_takes_only_the_default_fallback_routing() {
        assert!(super::anthropic_accepts_fallback_models("claude-opus-5-5"));
        assert!(super::anthropic_accepts_fallback_models("claude-fable-5-1"));
        assert!(!super::anthropic_accepts_fallback_models(
            "claude-sonnet-5-5"
        ));
        assert!(!super::anthropic_accepts_fallback_models(
            "claude-haiku-5-5"
        ));
        assert!(!super::anthropic_accepts_fallback_models("claude-opus-4-8"));
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
