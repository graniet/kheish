use anyhow::{Result, bail};
use kheish_types::SessionOperatorConfig;

pub(crate) fn normalize_session_operator_config(
    mut config: SessionOperatorConfig,
) -> Result<SessionOperatorConfig> {
    config.display_name = normalize_prompt_text_field(
        "session operator display_name",
        config.display_name.as_deref(),
    )?;
    config.communication_style = normalize_prompt_text_field(
        "session operator communication_style",
        config.communication_style.as_deref(),
    )?;
    if config.enabled && !config.allow_notify && !config.allow_questions {
        bail!("session operator config must allow notify_operator or ask_operator when enabled");
    }
    if !config.enabled {
        return Ok(SessionOperatorConfig::default());
    }
    Ok(config)
}

fn normalize_prompt_text_field(path: &str, value: Option<&str>) -> Result<Option<String>> {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    validate_prompt_visible_operator_text(path, trimmed)?;
    Ok(Some(trimmed.to_string()))
}

fn validate_prompt_visible_operator_text(path: &str, value: &str) -> Result<()> {
    if kheish_runtime::redact_text(value) != value {
        bail!("{path} appears to contain secret material");
    }
    let lower = value.to_ascii_lowercase();
    if lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("://")
        || lower.contains("webhook")
        || lower.contains("chat_id")
        || lower.contains("chat id")
        || lower.contains("chat-id")
        || lower.contains("bot_token")
        || lower.contains("bot token")
        || contains_destination_keyword(&lower)
        || contains_contact_scheme(&lower)
        || contains_email_like_identifier(value)
        || contains_slack_like_identifier(value)
        || contains_handle_like_identifier(value)
        || is_bare_numeric_identifier(value)
    {
        bail!("{path} must not contain delivery addresses or token references");
    }
    Ok(())
}

fn contains_destination_keyword(lower: &str) -> bool {
    [
        "channel",
        "contact address",
        "connector",
        "delivery address",
        "destination",
        "direct message",
        "dm ",
        " dm",
        "operator address",
        "plugin",
        "reply address",
        "reply target",
        "reply-target",
        "route",
        "target address",
        "transport",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

fn contains_contact_scheme(lower: &str) -> bool {
    ["mailto:", "matrix:", "slack:", "sms:", "tel:", "tg:"]
        .iter()
        .any(|scheme| lower.contains(scheme))
}

fn contains_email_like_identifier(value: &str) -> bool {
    value.split_whitespace().any(|part| {
        let trimmed = part.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | ':' | '.' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\''
            )
        });
        let Some((local, domain)) = trimmed.split_once('@') else {
            return false;
        };
        !local.is_empty()
            && domain.contains('.')
            && domain.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            })
    })
}

fn contains_slack_like_identifier(value: &str) -> bool {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            let bytes = token.as_bytes();
            (9..=32).contains(&bytes.len())
                && matches!(bytes.first(), Some(b'C' | b'G' | b'D'))
                && bytes.get(1).is_some_and(u8::is_ascii_digit)
                && token
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        })
}

fn contains_handle_like_identifier(value: &str) -> bool {
    value.split_whitespace().any(|part| {
        let trimmed = part.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | ':' | '.' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\''
            )
        });
        let Some(rest) = trimmed
            .strip_prefix('@')
            .or_else(|| trimmed.strip_prefix('#'))
        else {
            return false;
        };
        !rest.is_empty()
            && rest
                .chars()
                .any(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    })
}

fn is_bare_numeric_identifier(value: &str) -> bool {
    let compact = value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    let digits = compact.strip_prefix('-').unwrap_or(&compact);
    digits.len() >= 6 && digits.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_session_operator_config_rejects_prompt_visible_secrets_and_addresses() {
        for value in [
            "Project sk-proj-secret",
            "Use https://example.com/hook",
            "telegram chat_id 123456",
            "ops@example.com",
            "C0123456789",
            "-1001234567890",
            "Slack #ops-alerts",
            "Telegram @ops_bot",
            "connector ops-bot",
            "reply target prod",
            "plugin/address",
            "tg://resolve?domain=ops_bot",
        ] {
            let error = normalize_session_operator_config(SessionOperatorConfig {
                enabled: true,
                display_name: Some(value.to_string()),
                allow_notify: false,
                allow_questions: true,
                ..Default::default()
            })
            .expect_err("prompt-visible operator fields should reject sensitive content");
            let message = error.to_string();
            assert!(
                message.contains("secret material")
                    || message.contains("delivery addresses or token references"),
                "{message}"
            );
        }
    }

    #[test]
    fn normalize_session_operator_config_canonicalizes_inactive_configs() {
        let normalized = normalize_session_operator_config(SessionOperatorConfig {
            enabled: false,
            display_name: Some("Project operator".to_string()),
            communication_style: Some("concise".to_string()),
            allow_notify: false,
            allow_questions: false,
        })
        .expect("inactive operator config should normalize");

        assert_eq!(normalized, SessionOperatorConfig::default());
    }
}
