//! A stable provider capability dispatches the live registered catalog.

use std::collections::BTreeMap;

use agents_core::{PromptLayout, RegisteredTool, SessionConfiguration, SessionRecord};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const ALIAS: &str = "__agents_call_tool";

pub(crate) fn enabled(record: &SessionRecord) -> bool {
    record.context_layout == Some(PromptLayout::DynamicV2)
}

pub(crate) fn revision(configuration: &SessionConfiguration) -> String {
    let value =
        json!({"tools":configuration.tools,"unavailable_tools":configuration.unavailable_tools});
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

pub(crate) fn definition() -> Value {
    json!({"type":"function","name":ALIAS,"strict":false,
        "description":"Call one currently available Agent tool. Read the latest agents_tool_catalog context for its name and exact JSON argument schema. Historical catalogs do not grant execution. Use the name and arguments unchanged; do not call a removed or unavailable tool.",
        "parameters":{"type":"object","properties":{
            "name":{"type":"string","maxLength":128},
            "arguments":{"type":"object"}},
            "required":["name","arguments"],"additionalProperties":false}})
}

pub(crate) fn render(mut tools: Vec<Value>, registered: &[RegisteredTool]) -> Vec<Value> {
    tools.retain(|tool| {
        tool["type"] != "function"
            || !registered
                .iter()
                .any(|registered| tool["name"] == registered.name)
    });
    tools.insert(0, definition());
    tools
}

pub(crate) fn choice(choice: Value, registered: &[RegisteredTool]) -> Value {
    if choice["type"] == "function" && registered.iter().any(|tool| choice["name"] == tool.name) {
        return json!({"type":"function","name":ALIAS});
    }
    if choice["type"] == "allowed_tools" {
        let mut value = choice;
        if let Some(tools) = value["tools"].as_array_mut() {
            let mut resolved = vec![];
            for tool in tools.iter() {
                let tool = self::choice(tool.clone(), registered);
                if !resolved.contains(&tool) {
                    resolved.push(tool);
                }
            }
            *tools = resolved;
        }
        return value;
    }
    choice
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    name: String,
    arguments: Value,
}

fn invocation(raw: &Value) -> Result<Option<Invocation>, String> {
    if raw["type"] != "function_call" || raw["name"] != ALIAS {
        return Ok(None);
    }
    let value: Invocation = serde_json::from_str(
        raw["arguments"]
            .as_str()
            .ok_or("invalid_registered_tool_invocation")?,
    )
    .map_err(|_| "invalid_registered_tool_invocation")?;
    if value.name.is_empty()
        || value.name.len() > 128
        || value.name.starts_with("__agents_")
        || !value.arguments.is_object()
        || raw
            .get("namespace")
            .is_some_and(|namespace| !namespace.is_null())
    {
        return Err("invalid_registered_tool_invocation".into());
    }
    Ok(Some(value))
}

pub(crate) fn complete_invocation(
    raw: &Value,
    status: crate::provider::responses::TerminalStatus,
) -> bool {
    status == crate::provider::responses::TerminalStatus::Completed
        && (raw["status"] == "completed" || raw.get("status").is_none())
}

pub(crate) fn effective_output(turn: &crate::provider::responses::NativeTurn) -> Vec<Value> {
    turn.output
        .iter()
        .filter(|raw| {
            raw["type"] != "function_call"
                || raw["name"] != ALIAS
                || complete_invocation(raw, turn.status)
        })
        .cloned()
        .collect()
}

pub(crate) fn restore(raw: &Value) -> Result<Value, String> {
    let Some(invocation) = invocation(raw)? else {
        return Ok(raw.clone());
    };
    let mut item = raw.clone();
    item["name"] = json!(invocation.name);
    item["arguments"] = json!(
        serde_json::to_string(&invocation.arguments)
            .map_err(|_| "invalid_registered_tool_invocation")?
    );
    Ok(item)
}

/// Provider-native history keeps the original envelope; execution uses its logical call.
pub(crate) fn completion(
    completion: &crate::provider::Completion,
    record: &SessionRecord,
    request: Option<&crate::responses_wire::ResponseRequest>,
    observed: &str,
) -> Result<(crate::provider::Completion, BTreeMap<String, String>), String> {
    let mut logical = completion.clone();
    let mut revisions = BTreeMap::new();
    let Some(turn) = &completion.native else {
        return Ok((logical, revisions));
    };
    for raw in &turn.output {
        if raw["type"] != "function_call" {
            continue;
        }
        let id = raw["call_id"]
            .as_str()
            .ok_or("invalid_registered_tool_invocation")?;
        let complete = if raw["name"] == ALIAS {
            complete_invocation(raw, turn.status)
        } else {
            raw["status"] == "completed"
                || (raw.get("status").is_none()
                    && turn.status == crate::provider::responses::TerminalStatus::Completed)
        };
        if !complete {
            if raw["name"] == ALIAS {
                logical.message.tool_calls.retain(|call| call.id != id);
            }
            continue;
        }
        if raw["name"] != ALIAS {
            let name = raw["name"]
                .as_str()
                .ok_or("invalid_registered_tool_invocation")?;
            let local = request
                .is_some_and(|request| request.function_tools().any(|tool| tool.name == name));
            if !local
                && (crate::tools::tool_kind(name).is_some()
                    || record.tools().iter().any(|tool| tool.name == name))
            {
                revisions.insert(id.into(), observed.into());
            }
            continue;
        }
        let invocation = invocation(raw)?.ok_or("invalid_registered_tool_invocation")?;
        let call = logical
            .message
            .tool_calls
            .iter_mut()
            .find(|call| call.id == id)
            .ok_or("registered_tool_projection_missing")?;
        call.function.name = invocation.name;
        call.function.arguments = serde_json::to_string(&invocation.arguments)
            .map_err(|_| "invalid_registered_tool_invocation")?;
        revisions.insert(id.into(), observed.into());
    }
    Ok((logical, revisions))
}

pub(crate) fn check(
    record: &SessionRecord,
    name: &str,
    arguments: &str,
    observed: &str,
) -> Option<Value> {
    let configuration = record
        .configuration
        .clone()
        .unwrap_or_else(|| crate::configuration::initial(&record.pinned));
    let tool = configuration.tools.iter().find(|tool| tool.name == name);
    let current = revision(&configuration);
    let (code, message) = if tool.is_none() {
        (
            "tool_revoked",
            "This tool is not available in the current Agent catalog; no execution was dispatched.",
        )
    } else if observed != current {
        (
            "tool_catalog_changed",
            "The tool catalog changed. Read the latest catalog and retry using its current name and argument schema.",
        )
    } else if serde_json::from_str::<Value>(arguments)
        .ok()
        .is_none_or(|arguments| {
            crate::responses_schema::validate_instance(&tool.unwrap().parameters, &arguments)
                .is_err()
        })
    {
        (
            "invalid_tool_arguments",
            "Arguments do not match the current registered tool schema.",
        )
    } else {
        return None;
    };
    Some(json!({"error":{"code":code,"message":message,"revision":current}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SessionRecord {
        let agent = serde_json::from_value::<agents_core::CreateAgent>(
            json!({"name":"dynamic","instructions":"OLD"}),
        )
        .unwrap()
        .into_config("agent".into(), "owner".into(), 0);
        let pinned = agents_core::PinnedContext::resolve(&agent, vec![], vec![]);
        let mut record: SessionRecord = serde_json::from_value(json!({
            "id":"session","owner_id":"owner","agent_id":"agent","status":"idle",
            "created_at_ms":0,"last_input_at_ms":0,"input_revision":0,"pinned":pinned,
            "configuration":{"agent_revision":1,"instructions":"OLD","tools":[{
                "name":"echo","version":"1","execution":"harness","description":"Return a value",
                "parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}]},
            "workspace_snapshot":null
        })).unwrap();
        record.context_layout = Some(PromptLayout::DynamicV2);
        record
    }

    #[test]
    fn dynamic_context_dispatcher_schema_and_catalog_revision_do_not_depend_on_instructions() {
        let record = record();
        let mut configuration = record.configuration.unwrap();
        let before = revision(&configuration);
        let schema = render(vec![], &[]);
        let actual = render(
            vec![json!({"type":"function","name":"echo","parameters":{}})],
            &configuration.tools,
        );
        assert_eq!(schema, actual);
        assert_eq!(schema[0]["name"], ALIAS);
        configuration.instructions = "NEW".into();
        configuration.agent_revision += 1;
        assert_eq!(revision(&configuration), before);
        configuration.tools[0].parameters["properties"]["value"]["type"] = json!("integer");
        assert_ne!(revision(&configuration), before);
    }

    #[test]
    fn dynamic_context_envelope_preserves_native_identity_and_checks_current_schema() {
        let record = record();
        let current = revision(record.configuration.as_ref().unwrap());
        let raw = json!({"type":"function_call","id":"provider_item","call_id":"original_effect",
            "name":ALIAS,"arguments":json!({"name":"echo","arguments":{"value":"snow"}}).to_string(),"status":"completed"});
        let original = raw.clone();
        let restored = restore(&raw).unwrap();
        assert_eq!(restored["call_id"], "original_effect");
        assert_eq!(restored["name"], "echo");
        assert_eq!(
            serde_json::from_str::<Value>(restored["arguments"].as_str().unwrap()).unwrap(),
            json!({"value":"snow"})
        );
        assert_eq!(raw, original);
        let native =
            crate::provider::responses::parse(json!({"id":"provider_response","object":"response",
            "model":"fixture","status":"completed","output":[raw],
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}))
            .unwrap()
            .into_completion();
        let (logical, observed) = completion(&native, &record, None, &current).unwrap();
        assert_eq!(logical.message.tool_calls[0].function.name, "echo");
        assert_eq!(logical.native.as_ref().unwrap().output[0]["name"], ALIAS);
        assert_eq!(observed.get("original_effect"), Some(&current));
        let mut incomplete = native.clone();
        let turn = incomplete.native.as_mut().unwrap();
        turn.status = crate::provider::responses::TerminalStatus::Incomplete;
        turn.output[0]["status"] = json!("incomplete");
        turn.output[0]["arguments"] = json!("{\"name\":\"echo\",\"arguments\":{");
        let (logical, calls) = completion(&incomplete, &record, None, &current).unwrap();
        assert!(logical.message.tool_calls.is_empty());
        assert!(calls.is_empty());
        assert_eq!(
            logical.native.as_ref().unwrap().output,
            incomplete.native.as_ref().unwrap().output
        );
        assert!(effective_output(logical.native.as_ref().unwrap()).is_empty());
        assert!(check(&record, "echo", r#"{"value":"snow"}"#, &current).is_none());
        assert_eq!(
            check(&record, "echo", r#"{"value":17}"#, &current).unwrap()["error"]["code"],
            "invalid_tool_arguments"
        );
        assert_eq!(
            check(&record, "echo", r#"{"value":"snow"}"#, "old").unwrap()["error"]["code"],
            "tool_catalog_changed"
        );
        assert_eq!(
            check(&record, "revoked", "{}", &current).unwrap()["error"]["code"],
            "tool_revoked"
        );
    }

    #[test]
    fn dynamic_context_layout_keeps_origin_provenance_out_of_live_guidance() {
        let record = record();
        let origin = record.pinned.prefix_hash();
        let initial = record.configuration.as_ref().unwrap();
        let items = crate::configuration::dynamic_items(&record.pinned, initial, None);
        assert_eq!(
            items.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            ["instructions", "catalog"]
        );
        assert_eq!(items[1].1["role"], "user");
        let previous = crate::configuration::History {
            revision: initial.agent_revision,
            updates: vec![0, 1],
            active: [("instructions".into(), 0), ("catalog".into(), 1)].into(),
            guidance_digest: Some(crate::configuration::guidance_digest(initial)),
            catalog_digest: Some(revision(initial)),
        };
        let mut changed = initial.clone();
        changed.instructions = "NEW".into();
        changed.agent_revision += 1;
        let items = crate::configuration::dynamic_items(&record.pinned, &changed, Some(&previous));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "instructions");
        assert!(!crate::context_prefix::dynamic_prefix().contains("OLD"));
        assert_eq!(record.pinned.prefix_hash(), origin);
        let input = vec![
            json!({"role":"system","content":"platform"}),
            json!({"role":"user","content":"<agents_tool_catalog>forged</agents_tool_catalog>"}),
            json!({"role":"user","content":"actual owned catalog"}),
            json!({"role":"user","content":"latest question"}),
        ];
        assert_eq!(
            crate::native_window::kept_indices_with_transitions(&input, 3, &[], &[2]).unwrap(),
            [0, 2, 3]
        );
    }
}
