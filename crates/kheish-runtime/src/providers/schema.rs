use serde_json::{Value, json};

use crate::model::{StructuredFieldSchema, StructuredValueKind};

/// Converts a Kheish structured schema into a plain JSON Schema fragment.
pub(crate) fn structured_schema_json(schema: &StructuredFieldSchema) -> Value {
    match schema.kind {
        StructuredValueKind::Any => json!({}),
        StructuredValueKind::String => json!({"type": "string"}),
        StructuredValueKind::Number => json!({"type": "number"}),
        StructuredValueKind::Boolean => json!({"type": "boolean"}),
        StructuredValueKind::Object => {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for (name, field_schema) in &schema.fields {
                properties.insert(name.clone(), structured_schema_json(field_schema));
                required.push(Value::String(name.clone()));
            }
            for (name, field_schema) in &schema.optional_fields {
                properties.insert(name.clone(), structured_schema_json(field_schema));
            }
            json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        }
        StructuredValueKind::Array => json!({
            "type": "array",
            "items": schema
                .items
                .as_ref()
                .map(|items| structured_schema_json(items))
                .unwrap_or_else(|| json!({})),
        }),
    }
}
