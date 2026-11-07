//! What providers other than Anthropic take of a tool's JSON Schema.
//!
//! A tool definition carries its schema as the tool wrote it: Kheish's own
//! tools render theirs from their runtime schema, an MCP tool carries the
//! server's `inputSchema` with its enums, integers, nested descriptions and
//! limits. Anthropic takes it as written. The OpenAI-compatible routes and
//! Gemini take a subset of JSON Schema, so each gets the most of the schema
//! it supports: local `$ref`s inlined, then the keywords it takes.

use serde_json::{Map, Value, json};

/// How deep `$ref` inlining goes before it gives up on a node.
const MAX_INLINE_DEPTH: usize = 16;

/// The schema for "any JSON value" spelled out as an explicit type union:
/// OpenAI's validators reject a bare `{}`.
fn any_value_schema() -> Map<String, Value> {
    let Value::Object(map) =
        json!({ "type": ["object", "array", "string", "number", "boolean", "null"] })
    else {
        unreachable!("the any-value schema is an object");
    };
    map
}

/// Inlines the local references of a schema (`#/$defs/<name>`,
/// `#/definitions/<name>`) and drops the definitions, for providers that do
/// not resolve references. A recursive reference, an unknown one, or one
/// nested deeper than [`MAX_INLINE_DEPTH`] becomes an unconstrained node that
/// keeps its description.
pub(crate) fn inline_local_refs(schema: &Value) -> Value {
    let definitions = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if definitions.is_empty() && !contains_ref(schema) {
        return schema.clone();
    }
    let mut inlined = inline_node(schema, &definitions, &mut Vec::new());
    if let Value::Object(root) = &mut inlined {
        root.remove("$defs");
        root.remove("definitions");
    }
    inlined
}

fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.contains_key("$ref") || object.values().any(contains_ref),
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

fn inline_node(value: &Value, definitions: &Map<String, Value>, path: &mut Vec<String>) -> Value {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let name = reference
                    .strip_prefix("#/$defs/")
                    .or_else(|| reference.strip_prefix("#/definitions/"));
                let target = name.and_then(|name| definitions.get(name).map(|t| (name, t)));
                let mut resolved = match target {
                    Some((name, target))
                        if path.len() < MAX_INLINE_DEPTH && !path.iter().any(|n| n == name) =>
                    {
                        path.push(name.to_string());
                        let resolved = inline_node(target, definitions, path);
                        path.pop();
                        resolved.as_object().cloned().unwrap_or_default()
                    }
                    _ => Map::new(),
                };
                // Keywords beside the reference (a description, mostly) win.
                for (keyword, sibling) in object {
                    if keyword != "$ref" {
                        resolved.insert(keyword.clone(), inline_node(sibling, definitions, path));
                    }
                }
                return Value::Object(resolved);
            }
            Value::Object(
                object
                    .iter()
                    .map(|(keyword, child)| {
                        (keyword.clone(), inline_node(child, definitions, path))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline_node(item, definitions, path))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The structural subset of a schema every OpenAI-compatible route takes:
/// `type` (integers included), `description`, `properties`, `required`,
/// `additionalProperties`, `items`, `enum`, `const` and `anyOf` (`oneOf`
/// becomes `anyOf`), at every depth. Local references are inlined first.
/// Value constraints (lengths, ranges, patterns, formats) and annotations
/// are left out; a node that names no type, value set or variants becomes
/// the explicit any-value union, and an `array` node always carries `items`.
pub(crate) fn openai_compatible_tool_schema(schema: &Value) -> Value {
    let mut converted = openai_compatible_node(&inline_local_refs(schema));
    if let Value::Object(root) = &mut converted {
        root.insert("type".to_string(), Value::String("object".to_string()));
        root.entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    converted
}

fn openai_compatible_node(value: &Value) -> Value {
    let Value::Object(object) = value else {
        return Value::Object(any_value_schema());
    };
    let mut converted = Map::new();
    for (keyword, child) in object {
        match keyword.as_str() {
            "type" | "description" | "required" | "enum" | "const" => {
                converted.insert(keyword.clone(), child.clone());
            }
            "properties" => {
                let properties = child
                    .as_object()
                    .map(|properties| {
                        properties
                            .iter()
                            .map(|(name, property)| {
                                (name.clone(), openai_compatible_node(property))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                converted.insert(keyword.clone(), Value::Object(properties));
            }
            "items" if child.is_object() => {
                converted.insert(keyword.clone(), openai_compatible_node(child));
            }
            "additionalProperties" => {
                // A schema for the extra properties is more than these
                // routes take: they stay allowed, unconstrained.
                let allowed = child.as_bool().unwrap_or(true);
                converted.insert(keyword.clone(), Value::Bool(allowed));
            }
            "anyOf" | "oneOf" => {
                if let Some(variants) = child.as_array() {
                    converted.insert(
                        "anyOf".to_string(),
                        Value::Array(variants.iter().map(openai_compatible_node).collect()),
                    );
                }
            }
            _ => {}
        }
    }
    if !["type", "enum", "const", "anyOf"]
        .iter()
        .any(|keyword| converted.contains_key(*keyword))
    {
        converted.extend(any_value_schema());
    }
    let is_array = converted.get("type").and_then(Value::as_str) == Some("array");
    if is_array && !converted.contains_key("items") {
        converted.insert("items".to_string(), Value::Object(any_value_schema()));
    }
    Value::Object(converted)
}

/// A schema prepared for Gemini's OpenAPI subset: local references inlined
/// and a nullable type (`["string", "null"]`) spelled `type` plus
/// `nullable: true`. The Gemini driver then keeps the keywords it takes.
pub(crate) fn gemini_compatible_tool_schema(schema: &Value) -> Value {
    let mut converted = inline_local_refs(schema);
    spell_nullable_types(&mut converted);
    converted
}

fn spell_nullable_types(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(kinds)) = object.get("type") {
                let non_null = kinds
                    .iter()
                    .filter(|kind| kind.as_str() != Some("null"))
                    .cloned()
                    .collect::<Vec<_>>();
                if non_null.len() == 1 && kinds.len() == 2 {
                    object.insert("type".to_string(), non_null[0].clone());
                    object.insert("nullable".to_string(), Value::Bool(true));
                }
            }
            for child in object.values_mut() {
                spell_nullable_types(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                spell_nullable_types(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fregate_like_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["service", "certificate"], "description": "The kind."},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "key": {"type": "string", "minLength": 3, "maxLength": 200, "title": "Key"},
                "next_step": {"$ref": "#/$defs/NextStep", "description": "What to do next."},
                "mode": {"oneOf": [{"type": "string"}, {"type": "null"}]},
                "meta": {"type": "object", "additionalProperties": {"type": "string"}},
                "tags": {"type": "array"},
                "anything": {"description": "No type."}
            },
            "required": ["kind", "key", "next_step"],
            "additionalProperties": false,
            "$defs": {
                "NextStep": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["remediate", "dismiss"]},
                        "detail": {"type": "string", "description": "At most 500 characters."}
                    },
                    "required": ["action", "detail"],
                    "additionalProperties": false
                }
            }
        })
    }

    #[test]
    fn openai_compatible_schemas_keep_the_structure_and_drop_the_rest() {
        assert_eq!(
            openai_compatible_tool_schema(&fregate_like_schema()),
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["service", "certificate"], "description": "The kind."},
                    "port": {"type": "integer"},
                    "key": {"type": "string"},
                    "next_step": {
                        "type": "object",
                        "description": "What to do next.",
                        "properties": {
                            "action": {"type": "string", "enum": ["remediate", "dismiss"]},
                            "detail": {"type": "string", "description": "At most 500 characters."}
                        },
                        "required": ["action", "detail"],
                        "additionalProperties": false
                    },
                    "mode": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "meta": {"type": "object", "additionalProperties": true},
                    "tags": {
                        "type": "array",
                        "items": {"type": ["object", "array", "string", "number", "boolean", "null"]}
                    },
                    "anything": {
                        "description": "No type.",
                        "type": ["object", "array", "string", "number", "boolean", "null"]
                    }
                },
                "required": ["kind", "key", "next_step"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn openai_compatible_schemas_leave_kheish_schemas_alone() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file."},
                "limit": {"type": "number"},
                "items": {"type": "array", "items": {"type": "string"}},
                "options": {"type": "object", "additionalProperties": true}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        assert_eq!(openai_compatible_tool_schema(&schema), schema);
    }

    #[test]
    fn recursive_references_become_unconstrained_nodes() {
        let schema = json!({
            "type": "object",
            "properties": {"tree": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {"children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}}
                }
            }
        });
        assert_eq!(
            inline_local_refs(&schema),
            json!({
                "type": "object",
                "properties": {
                    "tree": {
                        "type": "object",
                        "properties": {"children": {"type": "array", "items": {}}}
                    }
                }
            })
        );
    }

    #[test]
    fn gemini_schemas_spell_nullable_types() {
        assert_eq!(
            gemini_compatible_tool_schema(&json!({
                "type": "object",
                "properties": {
                    "cursor": {"type": ["string", "null"]},
                    "any": {"type": ["object", "array", "string", "number", "boolean", "null"]}
                }
            })),
            json!({
                "type": "object",
                "properties": {
                    "cursor": {"type": "string", "nullable": true},
                    "any": {"type": ["object", "array", "string", "number", "boolean", "null"]}
                }
            })
        );
    }
}
