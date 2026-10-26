//! Public MCP items are deliberately separate from private server capabilities.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub(crate) fn item_id(prefix: &str, operation: &str) -> String {
    format!("{prefix}_{:x}", Sha256::digest(operation.as_bytes()))
}

pub(crate) fn approval(id: &str, server: &str, name: &str, arguments: &str) -> Value {
    json!({"id":id,"type":"mcp_approval_request","server_label":server,
        "name":name,"arguments":arguments})
}

pub(crate) fn call(
    id: &str,
    server: &str,
    name: &str,
    arguments: &str,
    approval_id: Option<&str>,
    output: Option<&str>,
    error: Option<Value>,
) -> Value {
    let status = if error.is_some() {
        "failed"
    } else {
        "completed"
    };
    let mut item = json!({"id":id,"type":"mcp_call","server_label":server,
        "name":name,"arguments":arguments,"status":status,"output":output,"error":error});
    if let Some(approval_id) = approval_id {
        item["approval_request_id"] = json!(approval_id);
    }
    item
}

pub(crate) fn pending(item: &Value) -> Option<(String, String, Value)> {
    if item["type"] != "mcp_approval_request" {
        return None;
    }
    let id = bounded(item.get("id")?, 256)?;
    let label = bounded(item.get("server_label")?, 128)?;
    let name = bounded(item.get("name")?, 256)?;
    let arguments = bounded(item.get("arguments")?, MAX_ARGUMENT_BYTES)?;
    if !serde_json::from_str::<Value>(arguments).ok()?.is_object() {
        return None;
    }
    Some((
        id.into(),
        format!("{label}/{name}"),
        approval(id, label, name, arguments),
    ))
}

pub(crate) fn validate_result(details: &Value, input: &Value) -> Result<(), &'static str> {
    let (id, _, _) = pending(details).ok_or("invalid_mcp_approval_request")?;
    let fields = input.as_object().ok_or("invalid_mcp_approval_response")?;
    if fields.keys().any(|key| {
        !["type", "id", "approval_request_id", "approve", "reason"].contains(&key.as_str())
    }) || input["type"] != "mcp_approval_response"
        || input["approval_request_id"] != id
        || !input["approve"].is_boolean()
        || input.get("reason").is_some_and(|value| {
            !value.is_null()
                && !value
                    .as_str()
                    .is_some_and(|reason| reason.len() <= 4096 && !reason.contains('\0'))
        })
        || input
            .get("id")
            .is_some_and(|value| !value.is_null() && bounded(value, 256).is_none())
    {
        return Err("invalid_mcp_approval_response");
    }
    Ok(())
}

/// Used for canonical native-provider projection as well as adapter publication.
/// The caller owns the canonical item ID; vendor extensions are not public data.
pub(crate) fn public_item(item: &Value) -> Option<Value> {
    let keys: &[&str] = match item["type"].as_str()? {
        "mcp_approval_request" => &["id", "type", "arguments", "name", "server_label"],
        "mcp_approval_response" => &["id", "type", "approval_request_id", "approve", "reason"],
        "mcp_call" => &[
            "id",
            "type",
            "arguments",
            "name",
            "server_label",
            "status",
            "output",
            "error",
            "approval_request_id",
        ],
        "mcp_list_tools" => &["id", "type", "server_label", "tools", "error"],
        _ => return None,
    };
    let mut result = serde_json::Map::new();
    for key in keys {
        if let Some(value) = item.get(*key) {
            result.insert((*key).into(), value.clone());
        }
    }
    if item["type"] == "mcp_list_tools" {
        let tools = item["tools"].as_array()?;
        let mut projected = Vec::with_capacity(tools.len());
        for tool in tools {
            let mut fields = serde_json::Map::new();
            for key in ["name", "description", "input_schema", "annotations"] {
                if let Some(value) = tool.get(key) {
                    fields.insert(key.into(), value.clone());
                }
            }
            projected.push(Value::Object(fields));
        }
        result.insert("tools".into(), Value::Array(projected));
    }
    Some(Value::Object(result))
}

pub(crate) fn bounded(value: &Value, limit: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= limit && !text.contains('\0'))
}

pub(crate) fn initial_item(item: &Value) -> Option<Value> {
    let mut initial = public_item(item)?;
    match item["type"].as_str()? {
        "mcp_list_tools" => {
            initial["tools"] = json!([]);
            initial["error"] = Value::Null;
        }
        "mcp_call" => {
            initial["arguments"] = json!("");
            initial["status"] = json!("in_progress");
            initial["output"] = Value::Null;
            initial["error"] = Value::Null;
        }
        _ => {}
    }
    Some(initial)
}

pub(crate) fn phase_event(item: &Value, terminal: bool) -> Option<&'static str> {
    match (
        item["type"].as_str()?,
        terminal,
        item.get("error").is_some_and(|error| !error.is_null()),
    ) {
        ("mcp_list_tools", false, _) => Some("response.mcp_list_tools.in_progress"),
        ("mcp_list_tools", true, false) => Some("response.mcp_list_tools.completed"),
        ("mcp_list_tools", true, true) => Some("response.mcp_list_tools.failed"),
        ("mcp_call", false, _) => Some("response.mcp_call.in_progress"),
        ("mcp_call", true, false) => Some("response.mcp_call.completed"),
        ("mcp_call", true, true) => Some("response.mcp_call.failed"),
        _ => None,
    }
}

pub(crate) fn argument_events(item: &Value, index: usize) -> Vec<(&'static str, Value)> {
    if item["type"] != "mcp_call" {
        return vec![];
    }
    vec![
        (
            "response.mcp_call_arguments.delta",
            json!({"item_id":item["id"],"output_index":index,"delta":item["arguments"]}),
        ),
        (
            "response.mcp_call_arguments.done",
            json!({"item_id":item["id"],"output_index":index,"arguments":item["arguments"]}),
        ),
    ]
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_identity_and_decision_are_not_tool_outputs() {
        let request = approval("mcpr_1", "owned", "write", r#"{"path":"a"}"#);
        let approved =
            json!({"type":"mcp_approval_response","approval_request_id":"mcpr_1","approve":true});
        assert!(validate_result(&request, &approved).is_ok());
        let nullable = json!({"type":"mcp_approval_response","approval_request_id":"mcpr_1","approve":true,"id":null,"reason":null});
        assert!(validate_result(&request, &nullable).is_ok());
        let denied = json!({"type":"mcp_approval_response","approval_request_id":"mcpr_1","approve":false,"reason":"Do not write"});
        assert!(validate_result(&request, &denied).is_ok());
        for bad in [
            json!({"type":"mcp_approval_response","approval_request_id":"other","approve":true}),
            json!({"type":"mcp_approval_response","approval_request_id":"mcpr_1","approve":"true"}),
            json!({"type":"mcp_approval_response","approval_request_id":"mcpr_1","approve":true,"arguments":"replacement"}),
            json!({"type":"function_call_output","call_id":"mcpr_1","output":"approve"}),
        ] {
            assert!(validate_result(&request, &bad).is_err());
        }
    }

    #[test]
    fn public_projection_never_echoes_vendor_capabilities() {
        let mut value = approval("mcpr_1", "owned", "read", "{}");
        value["authorization"] = json!("private-bearer");
        value["__agents_mcp"] = json!({"ciphertext":"private-state"});
        assert!(!public_item(&value).unwrap().to_string().contains("private"));
        assert!(!pending(&value).unwrap().2.to_string().contains("private"));
        let value = json!({"type":"mcp_list_tools","id":"mcpl_1","server_label":"owned","tools":[{"name":"read","input_schema":{},"private":"do-not-echo"}],"headers":{"Authorization":"secret"}});
        let public = public_item(&value).unwrap().to_string();
        assert!(!public.contains("secret"));
        assert!(!public.contains("do-not-echo"));
    }

    #[test]
    fn operation_identity_is_stable_but_not_shared_by_distinct_calls() {
        assert_eq!(item_id("mcpr", "first"), item_id("mcpr", "first"));
        assert_ne!(item_id("mcpr", "first"), item_id("mcpr", "second"));
    }
}
