//! Claude model identifiers and the per-generation facts the runtime needs.
//!
//! Claude model ids name a family and a version (`claude-opus-5-5`,
//! `claude-sonnet-4-5-20250929`). Cloud platforms decorate them: Amazon Bedrock
//! prefixes `anthropic.` (or a region such as `us.anthropic.`) and may append a
//! `-v1:0` revision, Google Vertex AI separates a snapshot date with `@`, and
//! some clients append a context variant such as `[1m]`. The parser below reads
//! the family and version through those decorations so every table keyed on a
//! generation treats all spellings of one model alike.

/// A Claude model family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClaudeModelFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

/// A Claude model generation parsed from a model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClaudeModelId {
    pub family: ClaudeModelFamily,
    /// The major version (`5` in `claude-opus-5-5`).
    pub major: u32,
    /// The minor version (`5` in `claude-opus-5-5`), absent for a `.0` release
    /// such as `claude-opus-5`.
    pub minor: Option<u32>,
}

impl ClaudeModelId {
    /// Returns whether this generation is `family major.minor` exactly.
    pub fn is(&self, family: ClaudeModelFamily, major: u32, minor: Option<u32>) -> bool {
        self.family == family && self.major == major && self.minor == minor
    }

    /// Returns the version as a comparable `(major, minor)` pair.
    pub fn version(&self) -> (u32, u32) {
        (self.major, self.minor.unwrap_or(0))
    }
}

/// Parses the family and version out of a Claude model id.
///
/// Returns `None` for ids that are not current-style Claude ids (for example
/// the retired `claude-3-5-sonnet-*` naming, `claude-mythos-preview`, or any
/// other provider's model).
pub fn parse_claude_model_id(model: &str) -> Option<ClaudeModelId> {
    let lower = model.trim().trim_matches('"').to_ascii_lowercase();
    let start = lower.find("claude-")?;
    let rest = &lower[start + "claude-".len()..];
    let rest = rest.split(['@', '[', ':', '/']).next()?;
    let mut tokens = rest.split('-');
    let family = match tokens.next()? {
        "opus" => ClaudeModelFamily::Opus,
        "sonnet" => ClaudeModelFamily::Sonnet,
        "haiku" => ClaudeModelFamily::Haiku,
        "fable" => ClaudeModelFamily::Fable,
        "mythos" => ClaudeModelFamily::Mythos,
        _ => return None,
    };
    let major = version_number(tokens.next()?)?;
    // A snapshot date (`20250929`) or a platform revision (`v1`) is not a minor
    // version: versions are one or two digits.
    let minor = tokens.next().and_then(version_number);
    Some(ClaudeModelId {
        family,
        major,
        minor,
    })
}

fn version_number(token: &str) -> Option<u32> {
    if token.is_empty() || token.len() > 2 || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// Returns the documented context window of a current Claude generation.
///
/// Claude 4.6 and later generations (except Claude Haiku 4.5) and every 5.x
/// generation serve a 1M-token window; `None` leaves older or unknown
/// generations to the caller's default.
pub fn claude_context_window(model: &str) -> Option<usize> {
    let id = parse_claude_model_id(model)?;
    claude_has_current_limits(&id).then_some(1_000_000)
}

/// Returns `(default, upper_limit)` output tokens for a current Claude
/// generation: 128K is the documented maximum output on Claude 4.6 and later
/// generations and every 5.x generation; Claude Haiku 4.5 keeps 64K.
pub fn claude_max_output_tokens(model: &str) -> Option<(u32, u32)> {
    let id = parse_claude_model_id(model)?;
    claude_has_current_limits(&id).then_some((64_000, 128_000))
}

fn claude_has_current_limits(id: &ClaudeModelId) -> bool {
    if id.major >= 5 {
        return true;
    }
    match id.family {
        ClaudeModelFamily::Opus | ClaudeModelFamily::Sonnet => id.version() >= (4, 6),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeModelFamily, ClaudeModelId, claude_context_window, claude_max_output_tokens,
        parse_claude_model_id,
    };

    fn id(family: ClaudeModelFamily, major: u32, minor: Option<u32>) -> Option<ClaudeModelId> {
        Some(ClaudeModelId {
            family,
            major,
            minor,
        })
    }

    #[test]
    fn parses_first_party_cloud_and_dated_spellings() {
        use ClaudeModelFamily::*;
        let cases = [
            ("claude-opus-5-5", id(Opus, 5, Some(5))),
            ("claude-opus-5", id(Opus, 5, None)),
            ("claude-fable-5-1", id(Fable, 5, Some(1))),
            ("claude-fable-5", id(Fable, 5, None)),
            ("claude-mythos-5-1", id(Mythos, 5, Some(1))),
            ("claude-sonnet-5-5", id(Sonnet, 5, Some(5))),
            ("claude-haiku-5-5", id(Haiku, 5, Some(5))),
            ("claude-opus-4-8", id(Opus, 4, Some(8))),
            ("claude-haiku-4-5-20251001", id(Haiku, 4, Some(5))),
            ("claude-opus-4-20250514", id(Opus, 4, None)),
            ("anthropic.claude-opus-5-5", id(Opus, 5, Some(5))),
            (
                "us.anthropic.claude-sonnet-5-5-v1:0",
                id(Sonnet, 5, Some(5)),
            ),
            ("anthropic.claude-opus-4-7-v1:0", id(Opus, 4, Some(7))),
            ("claude-opus-4-5@20251101", id(Opus, 4, Some(5))),
            ("claude-opus-4-6[1m]", id(Opus, 4, Some(6))),
            ("anthropic/claude-opus-5-5", id(Opus, 5, Some(5))),
            ("  \"CLAUDE-OPUS-5-5\" ", id(Opus, 5, Some(5))),
            ("claude-3-5-sonnet-20241022", None),
            ("claude-mythos-preview", None),
            ("gpt-5.4", None),
        ];
        for (model, expected) in cases {
            assert_eq!(parse_claude_model_id(model), expected, "{model}");
        }
    }

    #[test]
    fn current_generations_have_a_1m_window_and_128k_output() {
        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-fable-5-1",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-opus-4-8",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "anthropic.claude-opus-5-5",
        ] {
            assert_eq!(claude_context_window(model), Some(1_000_000), "{model}");
            assert_eq!(
                claude_max_output_tokens(model),
                Some((64_000, 128_000)),
                "{model}"
            );
        }
        for model in ["claude-haiku-4-5", "claude-sonnet-4-5", "claude-opus-4-5"] {
            assert_eq!(claude_context_window(model), None, "{model}");
            assert_eq!(claude_max_output_tokens(model), None, "{model}");
        }
    }
}
