//! Client schemas are untrusted input: validation must not acquire network or
//! filesystem capabilities, and regular-expression matching must remain bounded.
//! The mature validator implements JSON Schema; this module only sets that policy.
//! https://docs.rs/jsonschema/0.49.9/jsonschema/#webassembly-support

use jsonschema::{PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 8192;
const MAX_INSTANCE_BYTES: usize = 1024 * 1024;
const MAX_INSTANCE_DEPTH: usize = 64;
const MAX_INSTANCE_NODES: usize = 65536;

struct NoExternalResources;

impl Retrieve for NoExternalResources {
    fn retrieve(
        &self,
        _uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("External JSON Schema resources are not permitted".into())
    }
}

fn bounded_json(
    value: &Value,
    max_bytes: usize,
    max_depth: usize,
    max_nodes: usize,
) -> Result<(), String> {
    if serde_json::to_vec(value)
        .map_err(|_| "Invalid JSON value")?
        .len()
        > max_bytes
    {
        return Err("JSON value exceeds its validation byte limit".into());
    }
    let mut pending = vec![(value, 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > max_nodes || depth > max_depth {
            return Err("JSON value exceeds its validation complexity limit".into());
        }
        match value {
            Value::Object(object) => {
                pending.extend(object.values().map(|value| (value, depth + 1)))
            }
            Value::Array(array) => pending.extend(array.iter().map(|value| (value, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

// Walk schema positions, not example/const payloads which can legitimately have
// a property named "$ref" without asking the validator to resolve a resource.
fn local_references_only(schema: &Value) -> Result<(), String> {
    let mut pending = vec![schema];
    while let Some(schema) = pending.pop() {
        let Some(object) = schema.as_object() else {
            continue;
        };
        for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(reference) = object.get(keyword) {
                let reference = reference
                    .as_str()
                    .ok_or("Schema references must be strings")?;
                if !reference.starts_with('#') {
                    return Err("Only local JSON Schema references are supported".into());
                }
            }
        }
        for keyword in [
            "additionalProperties",
            "unevaluatedProperties",
            "propertyNames",
            "contains",
            "not",
            "if",
            "then",
            "else",
            "additionalItems",
            "unevaluatedItems",
            "contentSchema",
        ] {
            if let Some(value) = object.get(keyword) {
                pending.push(value);
            }
        }
        for keyword in [
            "$defs",
            "definitions",
            "properties",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
        ] {
            if let Some(values) = object.get(keyword).and_then(Value::as_object) {
                pending.extend(values.values().filter(|value| !value.is_array()));
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            match object.get(keyword) {
                Some(Value::Array(values)) => pending.extend(values),
                Some(value) if keyword == "items" => pending.push(value),
                _ => {}
            }
        }
    }
    Ok(())
}

fn compile(schema: &Value) -> Result<Validator, String> {
    bounded_json(schema, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES)?;
    local_references_only(schema)?;
    jsonschema::options()
        .with_retriever(NoExternalResources)
        .with_pattern_options(PatternOptions::regex())
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .build(schema)
        .map_err(|_| "Invalid, unsupported or unresolved JSON Schema".into())
}

pub fn validate_schema(schema: &Value) -> Result<(), String> {
    compile(schema).map(|_| ())
}

pub fn validate_instance(schema: &Value, instance: &Value) -> Result<(), String> {
    bounded_json(
        instance,
        MAX_INSTANCE_BYTES,
        MAX_INSTANCE_DEPTH,
        MAX_INSTANCE_NODES,
    )?;
    if compile(schema)?.is_valid(instance) {
        Ok(())
    } else {
        // ValidationError Display includes instance values, which must not leak
        // into shared logs or a different response's diagnostics.
        Err("Value does not match the required JSON Schema".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn validates_types_required_fields_and_additional_properties() {
        let schema = json!({
            "type":"object","properties":{"count":{"type":"integer","minimum":1}},
            "required":["count"],"additionalProperties":false,
        });
        assert!(validate_schema(&schema).is_ok());
        assert!(validate_instance(&schema, &json!({"count":1})).is_ok());
        for value in [
            json!({}),
            json!({"count":0}),
            json!({"count":"1"}),
            json!({"count":1,"other":true}),
        ] {
            assert!(validate_instance(&schema, &value).is_err());
        }
    }

    #[test]
    fn local_defs_and_recursive_structures_work_without_retrieval() {
        let schema = json!({
            "$defs":{"node":{
                "type":"object","properties":{
                    "value":{"type":"string"},
                    "children":{"type":"array","items":{"$ref":"#/$defs/node"}}
                },"required":["value"],"additionalProperties":false
            }},"$ref":"#/$defs/node"
        });
        assert!(validate_schema(&schema).is_ok());
        assert!(
            validate_instance(
                &schema,
                &json!({"value":"root","children":[{"value":"leaf"}]})
            )
            .is_ok()
        );
        assert!(
            validate_instance(&schema, &json!({"value":"root","children":[{"value":123}]}))
                .is_err()
        );
    }

    #[test]
    fn external_resources_are_rejected_even_in_unused_definitions() {
        for reference in [
            "https://example.invalid/schema",
            "http://127.0.0.1/schema",
            "file:///etc/passwd",
            "relative.json",
            "data:application/json,true",
        ] {
            for schema in [
                json!({"$ref":reference}),
                json!({"type":"object","$defs":{"unused":{"$ref":reference}}}),
                json!({"type":"object","properties":{"x":{"$dynamicRef":reference}}}),
            ] {
                assert!(validate_schema(&schema).is_err());
            }
        }
        let example = json!({"type":"object","examples":[{"$ref":"this-is-data"}]});
        assert!(validate_schema(&example).is_ok());
    }

    #[test]
    fn malformed_or_unresolved_schemas_fail_before_generation() {
        for schema in [
            json!({"type":"not-a-type"}),
            json!({"required":"wrong"}),
            json!({"$ref":"#/$defs/missing"}),
            json!({"properties":{"x":{"minimum":"zero"}}}),
            json!({"$schema":"https://example.invalid/custom-meta-schema","type":"string"}),
        ] {
            assert!(validate_schema(&schema).is_err());
        }
    }

    #[test]
    fn linear_regex_engine_rejects_backreferences_and_validates_patterns() {
        assert!(validate_schema(&json!({"type":"string","pattern":"^(a+)\\1$"})).is_err());
        let schema = json!({"type":"string","pattern":"^[a-z]+$"});
        assert!(validate_instance(&schema, &json!("hello")).is_ok());
        assert!(validate_instance(&schema, &json!("HELLO")).is_err());
    }

    #[test]
    fn byte_depth_and_error_privacy_bounds_are_enforced() {
        assert!(validate_schema(&json!({"description":"x".repeat(MAX_SCHEMA_BYTES)})).is_err());
        let mut deep = json!(true);
        for _ in 0..=MAX_SCHEMA_DEPTH {
            deep = json!({"not":deep});
        }
        assert!(validate_schema(&deep).is_err());
        assert!(
            validate_instance(
                &json!({"type":"string","maxLength":1}),
                &json!("secret-value")
            )
            .is_err()
        );
        let error =
            validate_instance(&json!({"type":"integer"}), &json!("private value")).unwrap_err();
        assert!(!error.contains("private value"));
    }

    #[test]
    fn arbitrary_precision_integer_comparisons_do_not_round() {
        let schema: Value =
            serde_json::from_str(r#"{"type":"integer","const":18446744073709551617}"#).unwrap();
        let expected: Value = serde_json::from_str("18446744073709551617").unwrap();
        let nearby: Value = serde_json::from_str("18446744073709551618").unwrap();
        assert!(validate_instance(&schema, &expected).is_ok());
        assert!(validate_instance(&schema, &nearby).is_err());
    }

    proptest! {
        #[test]
        fn numeric_bounds_agree_with_integer_ordering(min in -1000i64..1000, width in 0i64..1000, value in -2000i64..3000) {
            let schema = json!({"type":"integer","minimum":min,"maximum":min+width});
            prop_assert_eq!(validate_instance(&schema, &json!(value)).is_ok(), value >= min && value <= min+width);
        }
    }
}
