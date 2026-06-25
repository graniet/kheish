use std::collections::{BTreeSet, VecDeque};
use std::sync::{Mutex, OnceLock, RwLock};

use serde_json::Value;

use crate::{AuthProvider, AuthSlotRecord};

const MIN_DEBUG_REDACTION_TOKEN_CHARS: usize = 6;
const MAX_DEBUG_REDACTION_TOKEN_CHARS: usize = 4 * 1024;
const MAX_EPHEMERAL_DEBUG_REDACTION_TOKENS: usize = 4_096;

#[derive(Default)]
struct EphemeralRedactionTokens {
    set: BTreeSet<String>,
    order: VecDeque<String>,
}

/// Replaces the process-local auth-store debug redaction tokens.
///
/// The runtime debug scrubber reads these tokens in addition to its built-in
/// patterns so opaque daemon-managed secrets are still scrubbed in `full` debug
/// capture, even when they do not look like provider keys.
pub fn replace_auth_store_debug_redaction_tokens<I>(tokens: I)
where
    I: IntoIterator<Item = String>,
{
    let store = auth_store_tokens();
    let mut next = BTreeSet::new();
    for token in tokens {
        if let Some(token) = normalize_redaction_token(token) {
            next.insert(token);
        }
    }
    *store.write().expect("auth redaction rwlock poisoned") = next;
}

/// Registers one short-lived broker token for debug redaction.
pub fn register_ephemeral_debug_redaction_token(token: impl Into<String>) {
    let Some(token) = normalize_redaction_token(token.into()) else {
        return;
    };
    let mut tokens = ephemeral_tokens()
        .lock()
        .expect("auth ephemeral redaction mutex poisoned");
    if tokens.set.insert(token.clone()) {
        tokens.order.push_back(token);
    }
    while tokens.set.len() > MAX_EPHEMERAL_DEBUG_REDACTION_TOKENS {
        let Some(evicted) = tokens.order.pop_front() else {
            break;
        };
        tokens.set.remove(&evicted);
    }
}

/// Returns all auth-managed literal tokens that must be scrubbed from debug output.
pub fn debug_redaction_tokens() -> Vec<String> {
    let mut tokens = auth_store_tokens()
        .read()
        .expect("auth redaction rwlock poisoned")
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    tokens.extend(
        ephemeral_tokens()
            .lock()
            .expect("auth ephemeral redaction mutex poisoned")
            .set
            .iter()
            .cloned(),
    );
    tokens.into_iter().collect()
}

pub(crate) fn replace_auth_store_debug_redaction_tokens_for_records<'a>(
    records: impl IntoIterator<Item = &'a AuthSlotRecord>,
) {
    replace_auth_store_debug_redaction_tokens(
        records
            .into_iter()
            .flat_map(auth_record_debug_redaction_tokens),
    );
}

pub(crate) fn register_auth_record_debug_redaction_tokens(record: &AuthSlotRecord) {
    for token in auth_record_debug_redaction_tokens(record) {
        register_ephemeral_debug_redaction_token(token);
    }
}

fn auth_store_tokens() -> &'static RwLock<BTreeSet<String>> {
    static TOKENS: OnceLock<RwLock<BTreeSet<String>>> = OnceLock::new();
    TOKENS.get_or_init(|| RwLock::new(BTreeSet::new()))
}

fn ephemeral_tokens() -> &'static Mutex<EphemeralRedactionTokens> {
    static TOKENS: OnceLock<Mutex<EphemeralRedactionTokens>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(EphemeralRedactionTokens::default()))
}

fn auth_record_debug_redaction_tokens(record: &AuthSlotRecord) -> Vec<String> {
    let mut tokens = Vec::new();
    let collect_all_strings = record.provider == AuthProvider::Generic;
    collect_state_redaction_tokens(&record.state, collect_all_strings, &mut tokens);
    tokens
}

fn collect_state_redaction_tokens(
    value: &Value,
    collect_all_strings: bool,
    tokens: &mut Vec<String>,
) {
    match value {
        Value::String(text) if collect_all_strings => tokens.push(text.clone()),
        Value::String(_) => {}
        Value::Array(items) => {
            for item in items {
                collect_state_redaction_tokens(item, collect_all_strings, tokens);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                let collect_child_strings = collect_all_strings || is_sensitive_auth_state_key(key);
                collect_state_redaction_tokens(value, collect_child_strings, tokens);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn is_sensitive_auth_state_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    normalized == "token"
        || normalized == "api_key"
        || normalized == "api_secret"
        || normalized == "authorization"
        || normalized == "credential"
        || normalized.ends_with("_token")
        || normalized.ends_with("_secret")
        || normalized.contains("access_token")
        || normalized.contains("refresh_token")
        || normalized.contains("id_token")
        || normalized.contains("auth_token")
        || normalized.contains("bearer_token")
        || normalized.contains("client_secret")
        || normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("passphrase")
        || normalized.contains("private_key")
        || normalized.contains("privatekey")
}

fn normalize_redaction_token(token: String) -> Option<String> {
    let token = token.trim();
    if token.chars().count() < MIN_DEBUG_REDACTION_TOKEN_CHARS {
        return None;
    }
    if token.chars().count() > MAX_DEBUG_REDACTION_TOKEN_CHARS {
        return None;
    }
    Some(token.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use super::*;
    use crate::{AuthMode, AuthSlotId};

    fn redaction_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("redaction test mutex poisoned")
    }

    #[test]
    fn auth_record_debug_redaction_tokens_include_only_secret_material() {
        let _guard = redaction_test_lock();
        let generic = AuthSlotRecord {
            slot_id: AuthSlotId::new("sidecars.webhook.routes"),
            provider: AuthProvider::Generic,
            mode: AuthMode::OpaqueSecret,
            state: json!({"value": "plain-opaque-canary"}),
            updated_at_ms: 1,
        };
        let openai = AuthSlotRecord {
            slot_id: AuthSlotId::new("openai.prod"),
            provider: AuthProvider::OpenAi,
            mode: AuthMode::ApiKey,
            state: json!({
                "kind": "api_key",
                "api_key": "sk-live-canary",
                "organization": "org-visible",
            }),
            updated_at_ms: 1,
        };

        replace_auth_store_debug_redaction_tokens_for_records([&generic, &openai]);
        let tokens = debug_redaction_tokens();

        assert!(tokens.contains(&"plain-opaque-canary".to_string()));
        assert!(tokens.contains(&"sk-live-canary".to_string()));
        assert!(!tokens.contains(&"org-visible".to_string()));
    }

    #[test]
    fn ephemeral_debug_redaction_tokens_are_bounded() {
        let _guard = redaction_test_lock();
        replace_auth_store_debug_redaction_tokens(Vec::<String>::new());
        for index in 0..(MAX_EPHEMERAL_DEBUG_REDACTION_TOKENS + 8) {
            register_ephemeral_debug_redaction_token(format!("lease-token-{index:05}"));
        }

        let tokens = debug_redaction_tokens();
        assert_eq!(tokens.len(), MAX_EPHEMERAL_DEBUG_REDACTION_TOKENS);
        assert!(!tokens.contains(&"lease-token-00000".to_string()));
        assert!(tokens.contains(&format!(
            "lease-token-{:05}",
            MAX_EPHEMERAL_DEBUG_REDACTION_TOKENS + 7
        )));
    }
}
