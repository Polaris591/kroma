//! KR-H02: native argument validation shared by every tool.
//!
//! Each tool used to validate its own input, and some validated nothing at all. A schema the model
//! is shown but never enforced is decoration: the model can send a call that cannot work, and the
//! tool discovers it halfway through a side effect. This module is the single pre-dispatch check.
//!
//! It is deliberately a *shallow* validator: object shape, required fields, primitive types, and
//! bounded depth/size. It is not a JSON-Schema implementation and does not pretend to be one. The
//! point is to catch the actionable class of error (missing field, wrong type, unparseable body)
//! before anything runs, with a message the model can fix.

use crate::tool::definition::ToolDefinition;
use serde_json::Value;

/// Largest argument body accepted before parsing is refused outright.
pub const MAX_ARGUMENT_BYTES: usize = 1 << 20;
/// Deepest nesting inspected. Beyond this the call is refused rather than walked.
pub const MAX_ARGUMENT_DEPTH: usize = 32;

/// Validate `raw_arguments` against `definition`.
///
/// Returns a model-actionable message on failure; the message names the field and the expected shape
/// so the next turn can repair it without guessing.
pub fn validate_arguments(definition: &ToolDefinition, raw_arguments: &str) -> Result<(), String> {
    if raw_arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(format!(
            "arguments are {} bytes, above the {} byte limit",
            raw_arguments.len(),
            MAX_ARGUMENT_BYTES
        ));
    }
    let trimmed = raw_arguments.trim();
    if trimmed.is_empty() {
        // Several tools legitimately take no arguments; an empty body is an empty object.
        return validate_object(definition, &Value::Object(Default::default()), 0);
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("arguments are not valid JSON ({e}); send a JSON object"))?;
    validate_object(definition, &value, 0)
}

fn validate_object(definition: &ToolDefinition, value: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_ARGUMENT_DEPTH {
        return Err(format!(
            "arguments nest deeper than the {MAX_ARGUMENT_DEPTH} level limit"
        ));
    }
    let object = match value {
        Value::Object(map) => map,
        other => {
            return Err(format!(
                "arguments must be a JSON object, got {}",
                json_kind(other)
            ))
        }
    };

    let schema = &definition.parameters;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(field) {
                return Err(format!(
                    "missing required field '{field}' for tool '{}'",
                    definition.name
                ));
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, field_schema) in properties {
            let Some(actual) = object.get(name) else {
                continue;
            };
            if actual.is_null() {
                continue;
            }
            if let Some(expected) = field_schema.get("type").and_then(Value::as_str) {
                if !type_matches(expected, actual) {
                    return Err(format!(
                        "field '{name}' must be {expected}, got {}",
                        json_kind(actual)
                    ));
                }
            }
            if let Some(inner) = field_schema.get("items") {
                if let (Some(items), Some(expected)) = (actual.as_array(), inner.get("type")) {
                    if let Some(expected) = expected.as_str() {
                        for (i, item) in items.iter().enumerate() {
                            if !type_matches(expected, item) {
                                return Err(format!(
                                    "field '{name}[{i}]' must be {expected}, got {}",
                                    json_kind(item)
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // An unknown type name is not a licence to accept anything; the caller sees the mismatch.
        _ => true,
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(params: Value) -> ToolDefinition {
        ToolDefinition {
            name: "probe".to_string(),
            description: "probe".to_string(),
            parameters: params,
        }
    }

    #[test]
    fn a_missing_required_field_is_named() {
        let def = tool(
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
        );
        let err = validate_arguments(&def, "{}").unwrap_err();
        assert!(err.contains("path"), "{err}");
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn a_wrong_type_is_named_with_both_sides() {
        let def = tool(json!({"type":"object","properties":{"limit":{"type":"integer"}}}));
        let err = validate_arguments(&def, r#"{"limit":"ten"}"#).unwrap_err();
        assert!(err.contains("limit"), "{err}");
        assert!(err.contains("integer"), "{err}");
        assert!(err.contains("string"), "{err}");
    }

    #[test]
    fn unparseable_arguments_are_refused_before_any_tool_runs() {
        let def = tool(json!({"type":"object"}));
        let err = validate_arguments(&def, "{not json").unwrap_err();
        assert!(err.contains("valid JSON"), "{err}");
    }

    #[test]
    fn a_non_object_body_is_refused() {
        let def = tool(json!({"type":"object"}));
        let err = validate_arguments(&def, "[1,2,3]").unwrap_err();
        assert!(err.contains("JSON object"), "{err}");
    }

    #[test]
    fn a_valid_call_passes_and_an_empty_body_is_an_empty_object() {
        let def = tool(
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
        );
        assert!(validate_arguments(&def, r#"{"path":"a.rs"}"#).is_ok());
        let no_args = tool(json!({"type":"object"}));
        assert!(validate_arguments(&no_args, "").is_ok());
    }

    #[test]
    fn array_item_types_are_checked() {
        let def = tool(
            json!({"type":"object","properties":{"names":{"type":"array","items":{"type":"string"}}}}),
        );
        assert!(validate_arguments(&def, r#"{"names":["a","b"]}"#).is_ok());
        let err = validate_arguments(&def, r#"{"names":["a",1]}"#).unwrap_err();
        assert!(err.contains("names[1]"), "{err}");
    }

    #[test]
    fn an_oversized_body_is_refused_without_parsing() {
        let def = tool(json!({"type":"object"}));
        let huge = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        let err = validate_arguments(&def, &huge).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }
}
