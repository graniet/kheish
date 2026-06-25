//! Shared canonical encoding helpers for Kheish runtimes.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonicalizes a JSON value by sorting object keys recursively.
pub fn canonicalize_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json_value).collect()),
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonicalize_json_value(&map[&key]));
            }
            Value::Object(canonical)
        }
        _ => value.clone(),
    }
}

/// Computes a SHA-256 digest for a canonical JSON value.
pub fn digest_json_value(value: &Value) -> Result<String> {
    let canonical = canonicalize_json_value(value);
    let bytes = serde_json::to_vec(&canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Computes a SHA-256 digest for UTF-8 text.
pub fn digest_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Computes a SHA-256 digest for raw bytes.
pub fn digest_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Serializes a value to JSON before computing a canonical SHA-256 digest.
pub fn digest_serialize<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    digest_json_value(&value)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::{canonicalize_json_value, digest_bytes, digest_json_value, digest_text};

    #[test]
    fn canonicalization_sorts_object_keys_recursively() {
        let input = json!({
            "b": 1,
            "a": {
                "d": true,
                "c": false
            }
        });

        let canonical = canonicalize_json_value(&input);
        assert_eq!(canonical, json!({"a": {"c": false, "d": true}, "b": 1}));
    }

    #[test]
    fn json_digest_is_stable_across_key_order() -> Result<()> {
        let left = json!({"a": 1, "b": {"c": 2, "d": 3}});
        let right = json!({"b": {"d": 3, "c": 2}, "a": 1});
        assert_eq!(digest_json_value(&left)?, digest_json_value(&right)?);
        Ok(())
    }

    #[test]
    fn text_digest_changes_with_content() {
        assert_ne!(digest_text("hello"), digest_text("world"));
    }

    #[test]
    fn byte_digest_changes_with_content() {
        assert_ne!(digest_bytes(b"hello"), digest_bytes(b"world"));
    }
}
