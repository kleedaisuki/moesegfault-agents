//! Messages API translation keeps provider-specific continuation blocks out of
//! user-facing OpenAI chunks, but preserves them verbatim in the durable journal.
//! <https://platform.claude.com/docs/en/build-with-claude/streaming>

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{
    Completion, FunctionCall, MAX_COMPLETION_BYTES, Message, ProviderConfig, ToolCall, Usage,
};
use crate::tools::ToolDefinition;

const CONTENT_KEY: &str = "anthropic_content";

pub(super) fn request_body(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    stream: bool,
) -> Result<Value, String> {
    let max_tokens = config
        .max_output_tokens
        .filter(|limit| *limit > 0)
        .ok_or("Anthropic requires an output token limit")?;
    let mut system = Vec::new();
    let mut conversation = Vec::new();
    for message in messages {
        if matches!(message.role.as_str(), "system" | "developer") {
            system.push(json!({"type":"text", "text":text_content(&message.content)?}));
            continue;
        }
        let role = if message.role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        append_message(&mut conversation, role, content_blocks(message)?)?;
    }
    if conversation.is_empty() || conversation[0]["role"] != "user" {
        return Err("Anthropic requires an initial user message".into());
    }
    let mut body = json!({
        "model":config.model, "max_tokens":max_tokens,
        "messages":conversation, "stream":stream,
    });
    if let Some(first) = system.first_mut() {
        // The session pins this prefix. A stable explicit breakpoint also caches
        // preceding tools, but excludes later mutable compaction summaries.
        first["cache_control"] = json!({"type":"ephemeral"});
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(
            tools
                .iter()
                .map(|tool| json!({
                    "name":tool.function.name,
                    "description":tool.function.description,
                    "input_schema":tool.function.parameters,
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(body)
}

fn text_content(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .ok_or_else(|| "Anthropic requires text message content".into())
}

fn content_blocks(message: &Message) -> Result<Vec<Value>, String> {
    match message.role.as_str() {
        "user" => Ok(vec![
            json!({"type":"text", "text":text_content(&message.content)?}),
        ]),
        "tool" => {
            let id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or("Anthropic tool result requires a call ID")?;
            Ok(vec![json!({"type":"tool_result", "tool_use_id":id,
                "content":text_content(&message.content)?})])
        }
        "assistant" => assistant_blocks(message),
        _ => Err("unsupported Anthropic message role".into()),
    }
}

fn assistant_blocks(message: &Message) -> Result<Vec<Value>, String> {
    if let Some(value) = message.extra.get(CONTENT_KEY) {
        return value
            .as_array()
            .cloned()
            .ok_or_else(|| "invalid persisted Anthropic continuation".into());
    }
    let mut blocks = Vec::new();
    if let Some(text) = message.content.as_str().filter(|text| !text.is_empty()) {
        blocks.push(json!({"type":"text", "text":text}));
    }
    for call in &message.tool_calls {
        let input: Value = serde_json::from_str(&call.function.arguments)
            .map_err(|_| "invalid Anthropic tool arguments")?;
        if !input.is_object() {
            return Err("Anthropic tool arguments must be an object".into());
        }
        blocks.push(json!({"type":"tool_use", "id":call.id,
            "name":call.function.name, "input":input}));
    }
    if blocks.is_empty() {
        return Err("empty Anthropic assistant continuation".into());
    }
    Ok(blocks)
}

fn append_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) -> Result<(), String> {
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
        let content = last["content"]
            .as_array_mut()
            .ok_or("invalid Anthropic conversation content")?;
        content.extend(blocks);
    } else {
        messages.push(json!({"role":role, "content":blocks}));
    }
    Ok(())
}

pub(super) fn parse_completion(value: Value) -> Result<Completion, String> {
    if value["role"] != "assistant" {
        return Err("provider returned an invalid Anthropic assistant".into());
    }
    let content = value["content"]
        .as_array()
        .ok_or("provider returned invalid Anthropic content")?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in content {
        match block["type"].as_str() {
            Some("text") => text.push_str(
                block["text"]
                    .as_str()
                    .ok_or("invalid Anthropic text block")?,
            ),
            Some("tool_use") => calls.push(parse_tool(block)?),
            Some("thinking" | "redacted_thinking") => {}
            _ => return Err("unsupported Anthropic content block".into()),
        }
    }
    if calls.len() > 128 {
        return Err("provider emitted too many tool calls".into());
    }
    let mut ids = BTreeSet::new();
    if calls.iter().any(|call| !ids.insert(&call.id)) {
        return Err("provider returned duplicate Anthropic tool IDs".into());
    }
    let finish_reason = match value["stop_reason"].as_str() {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        Some("end_turn" | "stop_sequence") => "stop",
        Some("refusal") => "content_filter",
        _ => return Err("provider returned an unsupported Anthropic stop reason".into()),
    };
    let has_tools = !calls.is_empty();
    if has_tools != (finish_reason == "tool_calls") {
        return Err("Anthropic tool input did not complete".into());
    }
    let id = value["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Anthropic completion is missing its ID")?;
    let model = value["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .ok_or("Anthropic completion is missing its model")?;
    let usage = parse_usage(&value["usage"], id)?;
    let mut message = Message::text("assistant", text);
    message.tool_calls = calls;
    message.extra.insert(CONTENT_KEY.into(), json!(content));
    Ok(Completion {
        id: id.into(),
        model: model.into(),
        message,
        finish_reason: finish_reason.into(),
        usage,
    })
}

fn parse_tool(block: &Value) -> Result<ToolCall, String> {
    let id = block["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Anthropic tool call is missing its ID")?;
    let name = block["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or("Anthropic tool call is missing its name")?;
    if !block["input"].is_object() {
        return Err("Anthropic tool arguments must be an object".into());
    }
    Ok(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: block["input"].to_string(),
        },
    })
}

fn parse_usage(value: &Value, id: &str) -> Result<Usage, String> {
    let count = |name: &str| -> Result<u64, String> {
        match value.get(name) {
            None => Ok(0),
            Some(value) => value
                .as_u64()
                .ok_or_else(|| "invalid Anthropic token usage".into()),
        }
    };
    let input = count("input_tokens")?;
    let output = count("output_tokens")?;
    let cached = count("cache_read_input_tokens")?;
    let created = count("cache_creation_input_tokens")?;
    let prompt = input
        .checked_add(cached)
        .and_then(|total| total.checked_add(created))
        .ok_or("Anthropic token usage overflow")?;
    let total = prompt
        .checked_add(output)
        .ok_or("Anthropic token usage overflow")?;
    Ok(Usage {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: total,
        cached_tokens: cached,
        provider_request_id: Some(id.into()),
        reported: value["input_tokens"].is_u64() && value["output_tokens"].is_u64(),
        cost: None,
    })
}

#[derive(Default)]
pub(super) struct StreamAccumulator {
    message: Option<Value>,
    blocks: BTreeMap<u64, StreamBlock>,
    bytes: usize,
    done: bool,
}

struct StreamBlock {
    value: Value,
    input: String,
    stopped: bool,
}

impl StreamAccumulator {
    pub(super) fn push(&mut self, event: Value) -> Result<Option<String>, String> {
        self.bytes = self.bytes.saturating_add(event.to_string().len());
        if self.bytes > MAX_COMPLETION_BYTES {
            return Err("provider completion exceeds maximum size".into());
        }
        if self.done {
            return Err("Anthropic event followed message_stop".into());
        }
        match event["type"].as_str() {
            Some("error") => Err("provider reported a streaming error".into()),
            Some("message_start") => self.start_message(event["message"].clone()),
            Some("content_block_start") => self.start_block(&event),
            Some("content_block_delta") => self.delta(&event),
            Some("content_block_stop") => self.stop_block(&event),
            Some("message_delta") => self.message_delta(&event),
            Some("message_stop") => {
                self.require_message()?;
                self.require_closed_blocks()?;
                self.done = true;
                Ok(None)
            }
            // The protocol permits ping and future event types. Neither is a
            // message boundary or authority to execute a partially received tool.
            Some(_) => Ok(None),
            None => Err("Anthropic stream event is missing its type".into()),
        }
    }

    pub(super) fn is_done(&self) -> bool {
        self.done
    }

    pub(super) fn finish(self) -> Result<Completion, String> {
        if !self.done {
            return Err("provider stream ended before completion".into());
        }
        let mut message = self
            .message
            .ok_or("Anthropic stream is missing message_start")?;
        let mut content = Vec::new();
        for (expected, (index, block)) in self.blocks.into_iter().enumerate() {
            if index != expected as u64 || !block.stopped {
                return Err("incomplete Anthropic content blocks".into());
            }
            content.push(block.value);
        }
        message["content"] = json!(content);
        parse_completion(message)
    }

    fn require_message(&self) -> Result<(), String> {
        if self.message.is_none() {
            return Err("Anthropic stream is missing message_start".into());
        }
        Ok(())
    }

    fn require_closed_blocks(&self) -> Result<(), String> {
        if self.blocks.values().any(|block| !block.stopped) {
            return Err("incomplete Anthropic content block".into());
        }
        Ok(())
    }

    fn start_message(&mut self, message: Value) -> Result<Option<String>, String> {
        if self.message.is_some()
            || !message.is_object()
            || !message["content"]
                .as_array()
                .is_some_and(|content| content.is_empty())
        {
            return Err("invalid Anthropic message_start".into());
        }
        self.message = Some(message);
        Ok(None)
    }

    fn start_block(&mut self, event: &Value) -> Result<Option<String>, String> {
        self.require_message()?;
        let index = block_index(event)?;
        if self.blocks.contains_key(&index) {
            return Err("duplicate Anthropic content block".into());
        }
        let value = event["content_block"].clone();
        if !matches!(
            value["type"].as_str(),
            Some("text" | "tool_use" | "thinking" | "redacted_thinking")
        ) {
            return Err("unsupported Anthropic content block".into());
        }
        let text = (value["type"] == "text")
            .then(|| value["text"].as_str().map(str::to_owned))
            .flatten();
        self.blocks.insert(
            index,
            StreamBlock {
                value,
                input: String::new(),
                stopped: false,
            },
        );
        Ok(text)
    }

    fn delta(&mut self, event: &Value) -> Result<Option<String>, String> {
        self.require_message()?;
        let block = self
            .blocks
            .get_mut(&block_index(event)?)
            .filter(|block| !block.stopped)
            .ok_or("Anthropic delta has no open content block")?;
        let delta = &event["delta"];
        match delta["type"].as_str() {
            Some("text_delta") => {
                append_field(block, "text", "text", delta)?;
                Ok(delta["text"].as_str().map(str::to_owned))
            }
            Some("input_json_delta") => {
                require_block_type(block, "tool_use")?;
                block.input.push_str(
                    delta["partial_json"]
                        .as_str()
                        .ok_or("invalid Anthropic tool JSON delta")?,
                );
                Ok(None)
            }
            Some("thinking_delta") => {
                append_field(block, "thinking", "thinking", delta)?;
                Ok(None)
            }
            Some("signature_delta") => {
                append_field(block, "thinking", "signature", delta)?;
                Ok(None)
            }
            Some(_) => Ok(None),
            None => Err("invalid Anthropic content delta".into()),
        }
    }

    fn stop_block(&mut self, event: &Value) -> Result<Option<String>, String> {
        self.require_message()?;
        let block = self
            .blocks
            .get_mut(&block_index(event)?)
            .filter(|block| !block.stopped)
            .ok_or("Anthropic content stop has no open block")?;
        if block.value["type"] == "tool_use" && !block.input.is_empty() {
            block.value["input"] = serde_json::from_str(&block.input)
                .map_err(|_| "invalid Anthropic streamed tool JSON")?;
        }
        if block.value["type"] == "tool_use" {
            parse_tool(&block.value)?;
        }
        block.stopped = true;
        Ok(None)
    }

    fn message_delta(&mut self, event: &Value) -> Result<Option<String>, String> {
        self.require_message()?;
        self.require_closed_blocks()?;
        let message = self.message.as_mut().unwrap();
        let delta = event["delta"]
            .as_object()
            .ok_or("invalid Anthropic message delta")?;
        for (key, value) in delta {
            message[key] = value.clone();
        }
        if let Some(usage) = event["usage"].as_object() {
            if !message["usage"].is_object() {
                message["usage"] = json!({});
            }
            for (key, value) in usage {
                message["usage"][key] = value.clone();
            }
        }
        Ok(None)
    }
}

fn block_index(event: &Value) -> Result<u64, String> {
    event["index"]
        .as_u64()
        .filter(|index| *index < 128)
        .ok_or_else(|| "invalid Anthropic content block index".into())
}

fn require_block_type(block: &StreamBlock, expected: &str) -> Result<(), String> {
    if block.value["type"] != expected {
        return Err("Anthropic delta does not match its content block".into());
    }
    Ok(())
}

fn append_field(
    block: &mut StreamBlock,
    kind: &str,
    field: &str,
    delta: &Value,
) -> Result<(), String> {
    require_block_type(block, kind)?;
    let fragment = delta[field]
        .as_str()
        .ok_or("invalid Anthropic content fragment")?;
    match block.value.get_mut(field) {
        Some(Value::String(current)) => current.push_str(fragment),
        None => block.value[field] = json!(fragment),
        _ => return Err("invalid Anthropic content field".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_core::ProviderProtocol;
    use proptest::prelude::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: "synthetic-key".into(),
            model: "synthetic-model".into(),
            max_output_tokens: Some(1024),
            session_id: Some("not-an-Anthropic-field".into()),
            traceparent: None,
        }
    }

    fn message(content: Vec<Value>, reason: &str) -> Value {
        json!({"id":"msg-1", "model":"synthetic-model", "role":"assistant",
            "content":content, "stop_reason":reason,
            "usage":{"input_tokens":100,"output_tokens":10,
                "cache_read_input_tokens":30,"cache_creation_input_tokens":20}})
    }

    fn start() -> StreamAccumulator {
        let mut stream = StreamAccumulator::default();
        let mut initial = message(Vec::new(), "");
        initial["stop_reason"] = Value::Null;
        initial["usage"]["output_tokens"] = json!(0);
        stream
            .push(json!({"type":"message_start", "message":initial}))
            .unwrap();
        stream
    }

    fn start_block(stream: &mut StreamAccumulator, index: u64, block: Value) {
        stream
            .push(json!({"type":"content_block_start", "index":index, "content_block":block}))
            .unwrap();
    }

    fn stop_block(stream: &mut StreamAccumulator, index: u64) {
        stream
            .push(json!({"type":"content_block_stop", "index":index}))
            .unwrap();
    }

    fn stop_message(stream: &mut StreamAccumulator, reason: &str) {
        stream
            .push(
                json!({"type":"message_delta", "delta":{"stop_reason":reason},
                "usage":{"output_tokens":10}}),
            )
            .unwrap();
        stream.push(json!({"type":"message_stop"})).unwrap();
    }

    fn time_block() -> Value {
        json!({"type":"tool_use", "id":"call-1", "name":"time", "input":{}})
    }

    #[test]
    fn request_preserves_signed_blocks_and_groups_all_tool_results_before_steering() {
        let content = vec![
            json!({"type":"thinking", "thinking":"private reasoning", "signature":"signed-block"}),
            json!({"type":"redacted_thinking", "data":"opaque-block"}),
            json!({"type":"text", "text":"Checking time."}),
            time_block(),
            json!({"type":"tool_use", "id":"call-2", "name":"time", "input":{}}),
        ];
        let assistant = parse_completion(message(content.clone(), "tool_use"))
            .unwrap()
            .message;
        let messages = vec![
            Message::text("system", "Pinned instructions"),
            Message::text("user", "What time is it?"),
            assistant,
            Message::tool_result("call-1", "result-one"),
            Message::tool_result("call-2", "result-two"),
            Message::text("user", "Use UTC."),
        ];
        let body = request_body(
            &config(),
            &messages,
            &crate::tools::registered_tools(false),
            true,
        )
        .unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][1]["content"], json!(content));
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(body["messages"][2]["content"][1]["tool_use_id"], "call-2");
        assert_eq!(body["messages"][2]["content"][2]["text"], "Use UTC.");
        assert_eq!(body["system"][0]["text"], "Pinned instructions");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "time");
        assert!(body["tools"][0]["input_schema"].is_object());
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("session_id").is_none());
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn cached_and_created_input_are_part_of_context_usage_without_model_cost() {
        let result = parse_completion(message(
            vec![json!({"type":"text", "text":"Hello"})],
            "end_turn",
        ))
        .unwrap();
        assert_eq!(result.usage.prompt_tokens, 150);
        assert_eq!(result.usage.cached_tokens, 30);
        assert_eq!(result.usage.completion_tokens, 10);
        assert_eq!(result.usage.total_tokens, 160);
        assert!(result.usage.reported);
        assert!(result.usage.cost.is_none());
        let mut no_usage = message(Vec::new(), "end_turn");
        no_usage.as_object_mut().unwrap().remove("usage");
        assert!(!parse_completion(no_usage).unwrap().usage.reported);
        let mut invalid = message(Vec::new(), "end_turn");
        invalid["usage"]["input_tokens"] = json!(-1);
        assert!(parse_completion(invalid).is_err());
        let mut overflow = message(Vec::new(), "end_turn");
        overflow["usage"]["input_tokens"] = json!(u64::MAX);
        assert!(parse_completion(overflow).is_err());
    }

    #[test]
    fn cache_breakpoint_stays_on_the_pinned_prefix_when_summary_changes() {
        let mut messages = vec![
            Message::text("system", "Pinned instructions and skills"),
            Message::text("system", "Mutable compaction summary one"),
            Message::text("user", "Continue"),
        ];
        let first = request_body(&config(), &messages, &[], true).unwrap();
        messages[1] = Message::text("system", "Mutable compaction summary two");
        let next = request_body(&config(), &messages, &[], true).unwrap();
        assert_eq!(first["system"][0], next["system"][0]);
        assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
        assert_ne!(first["system"][1]["text"], next["system"][1]["text"]);
        assert!(first["system"][1].get("cache_control").is_none());
        assert!(next["system"][1].get("cache_control").is_none());
    }

    #[test]
    fn streamed_thinking_signature_tool_json_and_cumulative_usage_roundtrip() {
        let mut stream = start();
        start_block(&mut stream, 0, json!({"type":"thinking", "thinking":""}));
        for delta in [
            json!({"type":"thinking_delta", "thinking":"Plan"}),
            json!({"type":"signature_delta", "signature":"sig-"}),
            json!({"type":"signature_delta", "signature":"tail"}),
        ] {
            assert!(
                stream
                    .push(json!({"type":"content_block_delta", "index":0, "delta":delta}))
                    .unwrap()
                    .is_none()
            );
        }
        stop_block(&mut stream, 0);
        start_block(&mut stream, 1, time_block());
        stream
            .push(json!({"type":"content_block_delta", "index":1,
            "delta":{"type":"input_json_delta", "partial_json":"{}"}}))
            .unwrap();
        stop_block(&mut stream, 1);
        stream
            .push(json!({"type":"message_delta", "delta":{"stop_reason":null},
            "usage":{"output_tokens":5}}))
            .unwrap();
        stop_message(&mut stream, "tool_use");
        assert!(stream.is_done());
        let result = stream.finish().unwrap();
        assert_eq!(result.message.tool_calls[0].function.arguments, "{}");
        assert_eq!(
            result.message.extra[CONTENT_KEY][0]["signature"],
            "sig-tail"
        );
        assert_eq!(result.message.extra[CONTENT_KEY][0]["thinking"], "Plan");
        assert_eq!(result.usage.prompt_tokens, 150);
        assert_eq!(result.usage.completion_tokens, 10);
        assert_eq!(result.usage.total_tokens, 160);
    }

    #[test]
    fn text_delta_streams_immediately_but_tools_require_terminal_message_stop() {
        let mut stream = start();
        start_block(&mut stream, 0, json!({"type":"text", "text":"Hello"}));
        let text = stream
            .push(json!({"type":"content_block_delta", "index":0,
            "delta":{"type":"text_delta", "text":" world"}}))
            .unwrap();
        assert_eq!(text.as_deref(), Some(" world"));
        stop_block(&mut stream, 0);
        stream
            .push(
                json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":10}}),
            )
            .unwrap();
        assert!(!stream.is_done());
        assert!(stream.finish().is_err());
        let mut stream = start();
        start_block(&mut stream, 0, time_block());
        assert!(stream.push(json!({"type":"message_stop"})).is_err());
    }

    #[test]
    fn malformed_tool_fragments_and_mismatched_content_are_rejected() {
        let mut stream = start();
        start_block(&mut stream, 0, time_block());
        stream
            .push(json!({"type":"content_block_delta", "index":0,
            "delta":{"type":"input_json_delta", "partial_json":"{broken"}}))
            .unwrap();
        assert!(
            stream
                .push(json!({"type":"content_block_stop", "index":0}))
                .is_err()
        );
        let mut stream = start();
        start_block(&mut stream, 0, time_block());
        assert!(
            stream
                .push(json!({"type":"content_block_delta", "index":0,
            "delta":{"type":"text_delta", "text":"not-text"}}))
                .is_err()
        );
        let mut stream = start();
        assert!(
            stream
                .push(json!({"type":"content_block_start", "index":128,
            "content_block":time_block()}))
                .is_err()
        );
        assert!(parse_completion(message(vec![time_block()], "max_tokens")).is_err());
        assert!(parse_completion(message(Vec::new(), "tool_use")).is_err());
        assert!(parse_completion(message(Vec::new(), "pause_turn")).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn tool_json_fragment_boundaries_do_not_change_arguments(
            command in "[^\\p{C}]{0,80}", fragment_width in 1usize..17,
        ) {
            let expected = json!({"command":command});
            let input = expected.to_string();
            let characters = input.chars().collect::<Vec<_>>();
            let mut stream = start();
            start_block(&mut stream, 0,
                json!({"type":"tool_use", "id":"call-1", "name":"bash", "input":{}}));
            for fragment in characters.chunks(fragment_width) {
                let partial: String = fragment.iter().collect();
                stream.push(json!({"type":"content_block_delta", "index":0,
                    "delta":{"type":"input_json_delta", "partial_json":partial}})).unwrap();
                prop_assert!(!stream.is_done());
            }
            stream.push(json!({"type":"future_nonterminal_event"})).unwrap();
            stop_block(&mut stream, 0);
            stop_message(&mut stream, "tool_use");
            let completion = stream.finish().unwrap();
            let actual: Value = serde_json::from_str(
                &completion.message.tool_calls[0].function.arguments).unwrap();
            prop_assert_eq!(actual, expected);
        }
    }
}
