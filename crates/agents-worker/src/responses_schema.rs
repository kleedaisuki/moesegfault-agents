//! Client schemas are untrusted input: validation must not acquire network or
//! filesystem capabilities, and regular-expression matching must remain bounded.
//! The mature validator implements JSON Schema; this module only sets that policy.
//! https://docs.rs/jsonschema/0.49.9/jsonschema/#webassembly-support

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use jsonschema::{PatternOptions, Retrieve, Uri, Validator};
use percent_encoding::percent_decode_str;
use serde_json::Value;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 8192;
const MAX_INSTANCE_BYTES: usize = 1024 * 1024;
const MAX_INSTANCE_DEPTH: usize = 64;
const MAX_INSTANCE_NODES: usize = 65536;
const MAX_REQUEST_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_APPLICATIONS: usize = 4096;
const MAX_REFERENCE_DEPTH: usize = 64;
const MAX_PATTERNS: usize = 32;
const REGEX_BYTES: usize = 64 * 1024;
const MAX_VALIDATION_NODE_WORK: usize = 1_000_000;
const MAX_VALIDATION_BYTE_WORK: usize = 64 * 1024 * 1024;
const MAX_NUMBER_DIGITS: usize = 4096;
const MAX_SCHEMA_NUMBER_DIGITS: usize = 64 * 1024;
const MAX_REQUEST_NUMBER_DIGITS: usize = 256 * 1024;
const MAX_INSTANCE_NUMBER_DIGITS: usize = 64 * 1024;

struct JsonSize {
    bytes: usize,
    nodes: usize,
    number_digits: usize,
}

struct SchemaSize {
    bytes: usize,
    applications: usize,
    patterns: usize,
    number_digits: usize,
}

/// One budget covers client functions and structured output together, rather
/// than allowing each of 128 tools to consume an independent regex allocation.
pub struct SchemaBudget {
    bytes: usize,
    applications: usize,
    patterns: usize,
    number_digits: usize,
}

impl Default for SchemaBudget {
    fn default() -> Self {
        Self {
            bytes: MAX_REQUEST_SCHEMA_BYTES,
            applications: MAX_APPLICATIONS,
            patterns: MAX_PATTERNS,
            number_digits: MAX_REQUEST_NUMBER_DIGITS,
        }
    }
}

impl SchemaBudget {
    pub fn validate(&mut self, schema: &Value) -> Result<(), String> {
        let size = schema_size(schema)?;
        if size.bytes > self.bytes
            || size.applications > self.applications
            || size.patterns > self.patterns
            || size.number_digits > self.number_digits
        {
            return Err("Schemas exceed the combined request validation budget".into());
        }
        compile(schema)?;
        self.bytes -= size.bytes;
        self.applications -= size.applications;
        self.patterns -= size.patterns;
        self.number_digits -= size.number_digits;
        Ok(())
    }
}

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
    max_number_digits: usize,
) -> Result<JsonSize, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| "Invalid JSON value")?
        .len();
    if bytes > max_bytes {
        return Err("JSON value exceeds its validation byte limit".into());
    }
    let mut pending = vec![(value, 0)];
    let mut nodes = 0;
    let mut number_digits = 0;
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
            Value::Number(number) => {
                number_digits += expanded_number_digits(number)?;
                if number_digits > max_number_digits {
                    return Err("JSON numbers exceed the expanded numeric digit budget".into());
                }
            }
            _ => {}
        }
    }
    Ok(JsonSize {
        bytes,
        nodes,
        number_digits,
    })
}

fn expanded_number_digits(number: &serde_json::Number) -> Result<usize, String> {
    let lexeme = number.to_string();
    let (significand, exponent) = lexeme
        .split_once('e')
        .or_else(|| lexeme.split_once('E'))
        .unwrap_or((&lexeme, "0"));
    let mut magnitude = 0usize;
    for digit in exponent.trim_start_matches(['+', '-']).bytes() {
        if !digit.is_ascii_digit() {
            return Err("Invalid JSON number exponent".into());
        }
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'));
        if magnitude > MAX_NUMBER_DIGITS {
            return Err("JSON number exponent exceeds the supported magnitude".into());
        }
    }
    // Lexical length alone misses tiny inputs such as 1e1000000, which would
    // otherwise allocate a million-digit BigInt inside exact numeric checks.
    let digits = significand
        .bytes()
        .filter(u8::is_ascii_digit)
        .count()
        .saturating_add(magnitude);
    if digits > MAX_NUMBER_DIGITS {
        return Err("JSON number exceeds the expanded numeric digit limit".into());
    }
    Ok(digits)
}

// Walk schema positions, not example/const payloads which can legitimately have
// a property named "$ref" without asking the validator to resolve a resource.
fn subschemas(schema: &Value, definitions: bool) -> Vec<&Value> {
    let mut children = vec![];
    let Some(object) = schema.as_object() else {
        return children;
    };
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
            children.push(value);
        }
    }
    for keyword in [
        "properties",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(values) = object.get(keyword).and_then(Value::as_object) {
            children.extend(values.values().filter(|value| !value.is_array()));
        }
    }
    if definitions {
        for keyword in ["$defs", "definitions"] {
            if let Some(values) = object.get(keyword).and_then(Value::as_object) {
                children.extend(values.values());
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
        match object.get(keyword) {
            Some(Value::Array(values)) => children.extend(values),
            Some(value) if keyword == "items" => children.push(value),
            _ => {}
        }
    }
    children
}

fn reference_fragment(reference: &str) -> Result<Cow<'_, str>, String> {
    let fragment = reference
        .strip_prefix('#')
        .ok_or("Only local JSON Schema references are supported")?;
    percent_decode_str(fragment)
        .decode_utf8()
        .map_err(|_| "Invalid schema reference fragment".into())
}

fn reference_target<'a>(
    root: &'a Value,
    reference: &str,
    anchors: &BTreeMap<String, &'a Value>,
) -> Result<&'a Value, String> {
    let fragment = reference_fragment(reference)?;
    let target = if fragment.is_empty() {
        Some(root)
    } else if fragment.starts_with('/') {
        root.pointer(&fragment)
    } else {
        anchors.get(fragment.as_ref()).copied()
    };
    target.ok_or_else(|| "Unresolved local JSON Schema reference".into())
}

struct ApplicationGraph<'a> {
    root: &'a Value,
    anchors: BTreeMap<String, &'a Value>,
    active: BTreeSet<usize>,
    costs: BTreeMap<usize, usize>,
}

impl<'a> ApplicationGraph<'a> {
    fn cost(&mut self, schema: &'a Value, depth: usize) -> Result<usize, String> {
        if depth > MAX_REFERENCE_DEPTH {
            return Err("Schema reference nesting exceeds the supported limit".into());
        }
        let key = std::ptr::from_ref(schema) as usize;
        if let Some(cost) = self.costs.get(&key) {
            return Ok(*cost);
        }
        if !self.active.insert(key) {
            return Err("Cyclic JSON Schema references are not supported".into());
        }
        let mut children = subschemas(schema, false);
        for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(reference) = schema.get(keyword) {
                let reference = reference
                    .as_str()
                    .ok_or("Schema references must be strings")?;
                children.push(reference_target(self.root, reference, &self.anchors)?);
            }
        }
        let mut cost = 1;
        // Tiny allOf graphs can double repeated-reference evaluation at every
        // level. Count expanded applications, not only serialized schema nodes.
        for child in children {
            cost += self.cost(child, depth + 1)?;
            if cost > MAX_APPLICATIONS {
                return Err(
                    "Expanded JSON Schema exceeds the validation application budget".into(),
                );
            }
        }
        self.active.remove(&key);
        self.costs.insert(key, cost);
        Ok(cost)
    }
}

fn schema_size(schema: &Value) -> Result<SchemaSize, String> {
    let size = bounded_json(
        schema,
        MAX_SCHEMA_BYTES,
        MAX_SCHEMA_DEPTH,
        MAX_SCHEMA_NODES,
        MAX_SCHEMA_NUMBER_DIGITS,
    )?;
    let mut pending = vec![schema];
    let mut schemas = vec![];
    let mut seen = BTreeSet::new();
    let mut anchors = BTreeMap::new();
    let mut patterns = 0;
    while let Some(node) = pending.pop() {
        if !seen.insert(std::ptr::from_ref(node) as usize) {
            continue;
        }
        // Keep admission's reference graph identical to the validator's resource
        // graph; a nested resource would change the base of fragment references.
        if !std::ptr::eq(node, schema) && (node.get("$id").is_some() || node.get("id").is_some()) {
            return Err("Nested JSON Schema resource IDs are not supported".into());
        }
        for keyword in ["$anchor", "$dynamicAnchor"] {
            if let Some(anchor) = node.get(keyword) {
                let anchor = anchor.as_str().ok_or("Schema anchors must be strings")?;
                if anchors
                    .insert(anchor.to_owned(), node)
                    .is_some_and(|previous| !std::ptr::eq(previous, node))
                {
                    return Err("Schema anchor names must be unique".into());
                }
            }
        }
        patterns += usize::from(node.get("pattern").is_some());
        patterns += node
            .get("patternProperties")
            .and_then(Value::as_object)
            .map_or(0, |patterns| patterns.len());
        if patterns > MAX_PATTERNS {
            return Err("Schema exceeds the regular expression budget".into());
        }
        for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(reference) = node.get(keyword) {
                let fragment = reference_fragment(
                    reference
                        .as_str()
                        .ok_or("Schema references must be strings")?,
                )?;
                if fragment.is_empty() {
                    pending.push(schema);
                } else if fragment.starts_with('/') {
                    pending.push(
                        schema
                            .pointer(&fragment)
                            .ok_or("Unresolved local JSON Schema reference")?,
                    );
                }
            }
        }
        schemas.push(node);
        pending.extend(subschemas(node, true));
    }
    let mut graph = ApplicationGraph {
        root: schema,
        anchors,
        active: BTreeSet::new(),
        costs: BTreeMap::new(),
    };
    let mut applications = schemas.len();
    for node in schemas {
        applications = applications.max(graph.cost(node, 0)?);
    }
    if applications > MAX_APPLICATIONS {
        return Err("Schema exceeds the validation application budget".into());
    }
    Ok(SchemaSize {
        bytes: size.bytes,
        applications,
        patterns,
        number_digits: size.number_digits,
    })
}

fn compile(schema: &Value) -> Result<Validator, String> {
    jsonschema::options()
        .with_retriever(NoExternalResources)
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(REGEX_BYTES)
                .dfa_size_limit(REGEX_BYTES),
        )
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .build(schema)
        .map_err(|_| "Invalid, unsupported or unresolved JSON Schema".into())
}

pub fn validate_schema(schema: &Value) -> Result<(), String> {
    SchemaBudget::default().validate(schema)
}

pub fn validate_instance(schema: &Value, instance: &Value) -> Result<(), String> {
    let instance_size = bounded_json(
        instance,
        MAX_INSTANCE_BYTES,
        MAX_INSTANCE_DEPTH,
        MAX_INSTANCE_NODES,
        MAX_INSTANCE_NUMBER_DIGITS,
    )?;
    let schema_size = schema_size(schema)?;
    if schema_size.applications.saturating_mul(instance_size.nodes) > MAX_VALIDATION_NODE_WORK
        || schema_size.applications.saturating_mul(
            instance_size
                .bytes
                .saturating_add(instance_size.number_digits),
        ) > MAX_VALIDATION_BYTE_WORK
    {
        return Err("Value and schema exceed the combined validation work budget".into());
    }
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
    fn acyclic_local_defs_work_without_retrieval() {
        let schema = json!({
            "$defs":{"value":{"type":"string"},"record":{
                "type":"object","properties":{"value":{"$ref":"#/$defs/value"}},
                "required":["value"],"additionalProperties":false
            }},"type":"array","items":{"$ref":"#/$defs/record"}
        });
        assert!(validate_schema(&schema).is_ok());
        assert!(validate_instance(&schema, &json!([{"value":"root"},{"value":"leaf"}])).is_ok());
        assert!(validate_instance(&schema, &json!([{"value":123}])).is_err());
    }

    #[test]
    fn cyclic_references_are_an_explicit_resource_boundary() {
        for schema in [
            json!({"$ref":"#"}),
            json!({"$defs":{"node":{"type":"object","properties":{"next":{"$ref":"#/$defs/node"}}}},"$ref":"#/$defs/node"}),
            json!({"$defs":{"one":{"$ref":"#/$defs/two"},"two":{"$ref":"#/$defs/one"}},"$ref":"#/$defs/one"}),
        ] {
            assert!(validate_schema(&schema).unwrap_err().contains("Cyclic"));
        }
    }

    fn doubling_schema(depth: usize) -> Value {
        let mut defs = serde_json::Map::new();
        defs.insert("S0".into(), json!({"type":"integer"}));
        for level in 1..=depth {
            let reference = json!({"$ref":format!("#/$defs/S{}",level-1)});
            defs.insert(
                format!("S{level}"),
                json!({"allOf":[reference.clone(),reference]}),
            );
        }
        json!({"$defs":defs,"$ref":format!("#/$defs/S{depth}")})
    }

    #[test]
    fn small_raw_ref_dags_cannot_expand_to_exponential_validation_work() {
        let malicious = doubling_schema(31);
        assert!(serde_json::to_vec(&malicious).unwrap().len() < 4096);
        assert!(validate_schema(&malicious).is_err());
        let bounded = doubling_schema(8);
        assert!(validate_schema(&bounded).is_ok());
        assert!(validate_instance(&bounded, &json!(7)).is_ok());
        assert!(validate_instance(&bounded, &json!("wrong")).is_err());
    }

    #[test]
    fn request_patterns_and_compiled_regex_allocations_are_bounded() {
        let mut budget = SchemaBudget::default();
        let schema = json!({"type":"string","pattern":"^a+$"});
        for _ in 0..MAX_PATTERNS {
            assert!(budget.validate(&schema).is_ok());
        }
        assert!(budget.validate(&schema).is_err());
        assert!(validate_schema(&json!({"type":"string","pattern":"[a-z]{40000}"})).is_err());
    }

    #[test]
    fn reference_fragments_use_uri_decoding_and_json_pointer_escaping() {
        for (name, reference) in [
            ("with space", "#/$defs/with%20space"),
            ("a/b", "#/$defs/a~1b"),
        ] {
            let mut defs = serde_json::Map::new();
            defs.insert(name.into(), json!({"type":"string"}));
            let schema = json!({"$defs":defs,"$ref":reference});
            assert!(validate_instance(&schema, &json!("value")).is_ok());
            assert!(validate_instance(&schema, &json!(1)).is_err());
        }
        let anchored = json!({"$defs":{"Name":{"$anchor":"Name","type":"string"}},"$ref":"#Name"});
        assert!(validate_instance(&anchored, &json!("value")).is_ok());
    }

    #[test]
    fn schema_instance_product_has_a_work_budget() {
        let schema = doubling_schema(9);
        assert!(validate_schema(&schema).is_ok());
        let instance = json!("x".repeat(128 * 1024));
        assert!(
            validate_instance(&schema, &instance)
                .unwrap_err()
                .contains("work budget")
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

    #[test]
    fn astronomical_exponents_are_rejected_before_big_number_expansion() {
        for number in ["1e1000000", "1e-1000000", "1.5e999999999999999999999999999"] {
            let schema: Value = serde_json::from_str(&format!("{{\"minimum\":{number}}}")).unwrap();
            assert!(validate_schema(&schema).is_err());
            let instance: Value = serde_json::from_str(number).unwrap();
            assert!(validate_instance(&json!({"type":"number"}), &instance).is_err());
        }
        let ordinary: Value = serde_json::from_str("1e20").unwrap();
        assert!(validate_instance(&json!({"type":"number","minimum":0}), &ordinary).is_ok());
    }

    #[test]
    fn expanded_numeric_digits_are_bounded_in_aggregate_and_per_request() {
        let bound: Value = serde_json::from_str("1e4000").unwrap();
        let minimum = json!({"minimum":bound});
        let many = json!({"allOf":vec![minimum.clone(); 20]});
        assert!(serde_json::to_vec(&many).unwrap().len() < 1024);
        assert!(validate_schema(&many).is_err());
        let mut budget = SchemaBudget::default();
        for _ in 0..MAX_REQUEST_NUMBER_DIGITS / 4001 {
            assert!(budget.validate(&minimum).is_ok());
        }
        assert!(budget.validate(&minimum).is_err());
        let instance = Value::Array(vec![serde_json::from_str("1e4000").unwrap(); 20]);
        assert!(validate_instance(&json!({"type":"array"}), &instance).is_err());
    }

    proptest! {
        #[test]
        fn numeric_bounds_agree_with_integer_ordering(min in -1000i64..1000, width in 0i64..1000, value in -2000i64..3000) {
            let schema = json!({"type":"integer","minimum":min,"maximum":min+width});
            prop_assert_eq!(validate_instance(&schema, &json!(value)).is_ok(), value >= min && value <= min+width);
        }
    }
}
