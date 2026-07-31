//! Rewriting an MCP input schema into the strict form the provider requires.
//!
//! The provider emits `strict: true` and `ToolSpec::validate` enforces it
//! (`additionalProperties: false`, every property in `required`). An MCP schema
//! almost never complies, and an invalid spec kills the whole turn, so schemas
//! are normalized here; a tool whose schema resists is dropped by the caller
//! with a diagnostic rather than left to break the session.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// Bound of an exposed input schema: a server must not be able to flood the
/// prompt through its schemas any more than through its descriptions
/// (ARCHITECTURE 6).
pub const MAX_SCHEMA_BYTES: usize = 16_384;
/// Depth bound of the schema rewrite: a hostile server does not get to blow the
/// stack with nesting.
const MAX_SCHEMA_DEPTH: u32 = 24;

/// Rewrites an MCP input schema into the strict form the model API requires.
/// Returns `None` when the schema cannot be exposed at all (non-object root,
/// nesting past the depth bound).
///
/// A property that was optional is widened to accept `null`, the documented way
/// to keep it optional while `required` lists everything; `invoke` strips those
/// nulls again before the call, so the server never sees an argument the model
/// meant to omit.
pub fn strict_input_schema(schema: &Value) -> Option<Value> {
    let obj = schema.as_object()?;
    let mut root = obj.clone();
    match root.get("type") {
        // MCP mandates an object schema; a missing type is tolerated and pinned.
        None => {
            root.insert("type".to_string(), Value::String("object".to_string()));
        }
        Some(_) if type_includes_object(&root) => {}
        Some(_) => return None,
    }
    normalize(&Value::Object(root), 0)
}

fn normalize(node: &Value, depth: u32) -> Option<Value> {
    if depth > MAX_SCHEMA_DEPTH {
        return None;
    }
    let Some(obj) = node.as_object() else {
        return Some(node.clone());
    };
    let mut out = obj.clone();
    let originally_required: BTreeSet<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(Value::Object(props)) = obj.get("properties") {
        let mut rewritten = Map::new();
        for (key, value) in props {
            let mut child = normalize(value, depth + 1)?;
            if !originally_required.contains(key) {
                child = widen_nullable(child);
            }
            rewritten.insert(key.clone(), child);
        }
        out.insert("properties".to_string(), Value::Object(rewritten));
    }
    for key in ["items", "additionalItems", "contains"] {
        if let Some(value) = obj.get(key) {
            out.insert(key.to_string(), normalize(value, depth + 1)?);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(items)) = obj.get(key) {
            let mut rewritten = Vec::with_capacity(items.len());
            for item in items {
                rewritten.push(normalize(item, depth + 1)?);
            }
            out.insert(key.to_string(), Value::Array(rewritten));
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get(key) {
            let mut rewritten = Map::new();
            for (name, value) in defs {
                rewritten.insert(name.clone(), normalize(value, depth + 1)?);
            }
            out.insert(key.to_string(), Value::Object(rewritten));
        }
    }
    if type_includes_object(&out) {
        let names: Vec<Value> = out
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().map(Value::String).collect())
            .unwrap_or_default();
        if !out.contains_key("properties") {
            out.insert("properties".to_string(), Value::Object(Map::new()));
        }
        out.insert("additionalProperties".to_string(), Value::Bool(false));
        out.insert("required".to_string(), Value::Array(names));
    }
    Some(Value::Object(out))
}

/// Adds `null` to the declared type of an optional property. A property without a
/// declared type (a `$ref`, a bare `enum`) is left untouched: widening it would
/// mean guessing its shape.
fn widen_nullable(node: Value) -> Value {
    let Value::Object(mut obj) = node else {
        return node;
    };
    let widened = match obj.get("type") {
        Some(Value::String(single)) if single != "null" => Some(Value::Array(vec![
            Value::String(single.clone()),
            Value::String("null".to_string()),
        ])),
        Some(Value::Array(kinds)) if !kinds.iter().any(|kind| kind.as_str() == Some("null")) => {
            let mut kinds = kinds.clone();
            kinds.push(Value::String("null".to_string()));
            Some(Value::Array(kinds))
        }
        _ => None,
    };
    if let Some(widened) = widened {
        obj.insert("type".to_string(), widened);
    }
    Value::Object(obj)
}

fn type_includes_object(schema: &Map<String, Value>) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("object")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::ToolSpec;

    /// One required property and one optional one: the strict rewrite has to
    /// promote both to `required` and make the optional one nullable.
    fn object_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn strict_schema_requires_every_property_and_denies_extras() {
        let schema = strict_input_schema(&object_schema()).unwrap();
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, BTreeSet::from(["path", "limit"]));
        // Originally optional -> nullable, so the model can still omit it.
        assert_eq!(
            schema["properties"]["limit"]["type"],
            serde_json::json!(["integer", "null"])
        );
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn strict_schema_recurses_into_nested_objects() {
        let schema = strict_input_schema(&serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": { "kind": { "type": "string" } }
                }
            },
            "required": ["filter"]
        }))
        .unwrap();
        let nested = &schema["properties"]["filter"];
        assert_eq!(nested["additionalProperties"], Value::Bool(false));
        assert_eq!(nested["required"], serde_json::json!(["kind"]));
    }

    #[test]
    fn missing_type_is_pinned_and_non_object_root_is_refused() {
        let pinned = strict_input_schema(&serde_json::json!({"properties": {}})).unwrap();
        assert_eq!(pinned["type"], "object");
        assert!(strict_input_schema(&serde_json::json!({"type": "string"})).is_none());
        assert!(strict_input_schema(&serde_json::json!("nope")).is_none());
    }

    #[test]
    fn deep_nesting_is_refused_instead_of_blowing_the_stack() {
        let mut schema = serde_json::json!({"type": "object", "properties": {}});
        for _ in 0..(MAX_SCHEMA_DEPTH + 4) {
            schema = serde_json::json!({
                "type": "object",
                "properties": { "next": schema },
                "required": ["next"]
            });
        }
        assert!(strict_input_schema(&schema).is_none());
    }

    #[test]
    fn normalized_schemas_pass_the_provider_validation() {
        let schema = strict_input_schema(&object_schema()).unwrap();
        let spec = ToolSpec::function("mcp__files__read".to_string(), "d".to_string(), schema);
        spec.validate().unwrap();
    }
}
