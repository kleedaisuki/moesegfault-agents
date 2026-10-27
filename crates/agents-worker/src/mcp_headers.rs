//! MCP 2026-07-28 header projection, including Unicode and sentinel escaping.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ParameterHeader {
    name: String,
    path: Vec<String>,
    kind: String,
}

pub(crate) fn encode(value: &str) -> String {
    if value.trim_matches([' ', '\t']) == value
        && value
            .bytes()
            .all(|byte| byte == b'\t' || (b' '..=b'~').contains(&byte))
        && !(value.starts_with("=?base64?") && value.ends_with("?="))
    {
        return value.into();
    }
    format!("=?base64?{}?=", STANDARD.encode(value.as_bytes()))
}

pub(crate) fn discover(schema: &Value) -> Result<Vec<ParameterHeader>, &'static str> {
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    let mut budget = 4096;
    visit(schema, &[], true, 0, &mut result, &mut names, &mut budget)?;
    Ok(result)
}

fn visit(
    value: &Value,
    path: &[String],
    reachable: bool,
    depth: usize,
    headers: &mut Vec<ParameterHeader>,
    names: &mut BTreeSet<String>,
    budget: &mut usize,
) -> Result<(), &'static str> {
    if *budget == 0 || depth > 32 {
        return Err("mcp_schema_limit");
    }
    *budget -= 1;
    let Some(fields) = value.as_object() else {
        return Ok(());
    };
    if let Some(name) = fields.get("x-mcp-header") {
        let name = name
            .as_str()
            .filter(|name| header_name(name))
            .ok_or("invalid_mcp_parameter_header")?;
        let kind = fields
            .get("type")
            .and_then(Value::as_str)
            .ok_or("invalid_mcp_parameter_header")?;
        if !reachable
            || path.is_empty()
            || !matches!(kind, "integer" | "string" | "boolean")
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err("invalid_mcp_parameter_header");
        }
        headers.push(ParameterHeader {
            name: format!("Mcp-Param-{name}"),
            path: path.to_vec(),
            kind: kind.into(),
        });
    }
    for (key, child) in fields {
        if key == "properties" {
            let properties = child.as_object().ok_or("invalid_mcp_schema")?;
            for (property, schema) in properties {
                let mut path = path.to_vec();
                path.push(property.clone());
                visit(schema, &path, reachable, depth + 1, headers, names, budget)?;
            }
        } else if matches!(
            key.as_str(),
            "items"
                | "additionalProperties"
                | "contains"
                | "not"
                | "if"
                | "then"
                | "else"
                | "propertyNames"
        ) {
            visit(child, path, false, depth + 1, headers, names, budget)?;
        } else if matches!(key.as_str(), "allOf" | "anyOf" | "oneOf" | "prefixItems") {
            if let Some(children) = child.as_array() {
                for child in children {
                    visit(child, path, false, depth + 1, headers, names, budget)?;
                }
            }
        } else if matches!(
            key.as_str(),
            "$defs" | "definitions" | "patternProperties" | "dependentSchemas"
        ) {
            if let Some(children) = child.as_object() {
                for child in children.values() {
                    visit(child, path, false, depth + 1, headers, names, budget)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn extract(
    headers: &[ParameterHeader],
    arguments: &Value,
) -> Result<BTreeMap<String, String>, &'static str> {
    let mut result = BTreeMap::new();
    for header in headers {
        let mut value = arguments;
        for property in &header.path {
            value = value.get(property).unwrap_or(&Value::Null);
        }
        if value.is_null() {
            continue;
        }
        let text = match header.kind.as_str() {
            "string" => value.as_str().map(str::to_owned),
            "boolean" => value.as_bool().map(|value| value.to_string()),
            "integer" => integer_text(value),
            _ => None,
        }
        .ok_or("invalid_mcp_parameter_header_value")?;
        if text.len() > 8192 {
            return Err("mcp_parameter_header_limit");
        }
        result.insert(header.name.clone(), encode(&text));
    }
    Ok(result)
}

fn integer_text(value: &Value) -> Option<String> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let number = value.as_f64()?;
    if number.fract() != 0.0 || number.abs() > MAX_SAFE {
        return None;
    }
    Some(if number == 0.0 {
        "0".into()
    } else {
        format!("{number:.0}")
    })
}

fn header_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_projection_accepts_integral_json_numbers_without_loss() {
        let headers = discover(&json!({"type":"object","properties":{
            "n":{"type":"integer","x-mcp-header":"Count"}
        }}))
        .unwrap();
        for (number, expected) in [
            (json!(42.0), "42"),
            (json!(-0.0), "0"),
            (json!(9_007_199_254_740_991u64), "9007199254740991"),
        ] {
            assert_eq!(
                extract(&headers, &json!({"n":number})).unwrap()["Mcp-Param-Count"],
                expected
            );
        }
        for number in [
            json!(42.5),
            json!(9_007_199_254_740_992u64),
            json!(-9_007_199_254_740_992i64),
            json!("42"),
        ] {
            assert!(extract(&headers, &json!({"n":number})).is_err());
        }
    }

    #[test]
    fn sentinel_encoding_is_unambiguous_and_prevents_header_injection() {
        assert_eq!(encode("us-west1"), "us-west1");
        assert_eq!(encode("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(encode(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(encode("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(
            encode("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn projection_reads_only_statically_bound_primitive_paths() {
        let schema = json!({"type":"object","properties":{"outer":{"type":"object","properties":{
            "region":{"type":"string","x-mcp-header":"Region"},"number":{"type":"integer","x-mcp-header":"Count"},"flag":{"type":"boolean","x-mcp-header":"Flag"}
        }}}});
        let headers = discover(&schema).unwrap();
        let values = extract(
            &headers,
            &json!({"outer":{"region":"雪","number":42,"flag":true}}),
        )
        .unwrap();
        assert_eq!(values["Mcp-Param-Region"], "=?base64?6Zuq?=");
        assert_eq!(values["Mcp-Param-Count"], "42");
        assert_eq!(values["Mcp-Param-Flag"], "true");
        assert!(extract(&headers, &json!({"outer":{"number":9007199254740992_u64}})).is_err());
        assert!(extract(&headers, &json!({})).unwrap().is_empty());
    }

    #[test]
    fn invalid_annotations_cannot_silently_change_routing() {
        for schema in [
            json!({"type":"string","x-mcp-header":"Root"}),
            json!({"properties":{"a":{"type":"number","x-mcp-header":"Region"}}}),
            json!({"items":{"type":"object","properties":{"a":{"type":"string","x-mcp-header":"Region"}}}}),
            json!({"properties":{"a":{"type":"string","x-mcp-header":"Region"},"b":{"type":"string","x-mcp-header":"region"}}}),
            json!({"properties":{"a":{"type":"string","x-mcp-header":"bad\r\nName"}}}),
        ] {
            assert!(discover(&schema).is_err());
        }
    }
}
