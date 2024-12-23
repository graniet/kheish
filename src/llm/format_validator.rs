use jsonschema::Validator;
use serde_json::Value;

use crate::errors::Error;

/// Creates a JSON Schema validator from a schema string
///
/// # Arguments
/// * `schema_content` - The JSON Schema as a string
///
/// # Returns
/// * `Result<Validator, Error>` - The compiled validator on success, or an error
pub fn build_validator(schema_content: &str) -> Result<Validator, Error> {
    let schema = serde_json::from_str(schema_content)?;
    Ok(jsonschema::validator_for(&schema)?)
}

/// Validates a JSON string against a schema validator
///
/// # Arguments
/// * `schema` - The compiled JSON Schema validator
/// * `response` - The JSON string to validate
///
/// # Returns
/// * `Result<bool, Error>` - True if valid, false if invalid, or an error
pub fn validate_response(schema: &Validator, response: &str) -> Result<bool, Error> {
    let val: Value = serde_json::from_str(response)?;
    Ok(schema.is_valid(&val))
}