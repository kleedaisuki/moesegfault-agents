//! Stateless Responses admission rules shared by edge and canonical DO admission.

use std::collections::BTreeMap;

use crate::{
    provider::Message,
    responses_wire::{NormalizedInput, ResponseRequest},
    tools,
};

#[derive(Debug)]
pub(crate) struct InputError {
    pub code: &'static str,
    pub message: String,
}

pub(crate) fn validate(request: &ResponseRequest) -> Result<NormalizedInput, InputError> {
    let input = request
        .normalize_structure()
        .map_err(|message| InputError {
            code: "invalid_request_error",
            message,
        })?;
    if request.previous_response_id.is_none() && request.conversation.is_none() {
        validate_imported_history(&input.messages).map_err(|code| InputError {
            code,
            message: "Imported function calls and outputs must be complete, ordered groups".into(),
        })?;
    } else if input
        .messages
        .iter()
        .any(|message| message.role == "assistant" || !message.tool_calls.is_empty())
    {
        return Err(InputError {
            code: "history_replay_not_supported",
            message: "Continuation accepts only new input and matching function results".into(),
        });
    }
    if request.tools.iter().any(|tool| {
        tool.name.starts_with("__agents_")
            || (matches!(tool.kind.as_str(), "function" | "custom")
                && tools::tool_kind(&tool.name).is_some())
    }) {
        return Err(InputError {
            code: "tool_name_conflict",
            message: "Client tools cannot replace server tools".into(),
        });
    }
    Ok(input)
}

pub(crate) fn validate_imported_history(messages: &[Message]) -> Result<(), &'static str> {
    let mut pending = BTreeMap::<&str, bool>::new();
    for message in messages {
        if message.role == "tool" {
            let Some(id) = message.tool_call_id.as_deref() else {
                return Err("invalid_tool_outputs");
            };
            let Some(done) = pending.get_mut(id) else {
                return Err("invalid_tool_outputs");
            };
            if *done {
                return Err("duplicate_tool_output");
            }
            *done = true;
            continue;
        }
        if pending.values().any(|done| !*done) {
            return Err("incomplete_tool_history");
        }
        pending.clear();
        for call in &message.tool_calls {
            if pending.insert(&call.id, false).is_some() {
                return Err("duplicate_tool_call");
            }
        }
    }
    if pending.values().any(|done| !*done) {
        return Err("incomplete_tool_history");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request(value: serde_json::Value) -> ResponseRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn invalid_fresh_input_is_rejected_before_session_admission() {
        let orphan = request(json!({"model":"agent", "input":[{
            "type":"function_call_output", "call_id":"orphan", "output":"fake"
        }]}));
        assert_eq!(
            validate(&orphan).err().unwrap().code,
            "invalid_tool_outputs"
        );
        for tool in tools::registered_tools(true) {
            let name = tool.function.name;
            let shadow = request(json!({"model":"agent","input":"hello","tools":[{
                "type":"function","name":name,"parameters":{"type":"object"}
            }]}));
            assert_eq!(
                validate(&shadow).err().unwrap().code,
                "tool_name_conflict",
                "{name}"
            );
        }
    }

    #[test]
    fn complete_imported_history_and_continuation_outputs_remain_valid() {
        let imported = request(json!({"model":"agent","input":[
            {"role":"assistant","content":"Earlier answer"},
            {"type":"function_call","call_id":"call_1","name":"local","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":"result"},
            {"role":"user","content":"Continue"}
        ]}));
        assert!(validate(&imported).is_ok());
        let continuation = request(
            json!({"model":"agent","previous_response_id":"resp_parent", "input":[
                {"type":"function_call_output","call_id":"call_1","output":"result"}
            ]}),
        );
        assert!(validate(&continuation).is_ok());
        let replay = request(
            json!({"model":"agent","previous_response_id":"resp_parent", "input":[
                {"role":"assistant","content":"Replay"}, {"role":"user","content":"Continue"}
            ]}),
        );
        assert_eq!(
            validate(&replay).err().unwrap().code,
            "history_replay_not_supported"
        );
    }
}
