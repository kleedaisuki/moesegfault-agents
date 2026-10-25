//! Stateless upstream Responses transport. Session state remains harness-owned.

use std::{collections::BTreeMap, fmt};

use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

use super::{Completion, FunctionCall, Message, ProviderConfig, ToolCall, Usage};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const CONTROL_FIELDS: &[&str] = &[
    "max_output_tokens",
    "temperature",
    "top_p",
    "tool_choice",
    "parallel_tool_calls",
    "text",
    "reasoning",
    "truncation",
    "include",
    "top_logprobs",
    "prompt_cache_key",
    "prompt_cache_options",
    "prompt_cache_retention",
    "service_tier",
    "safety_identifier",
    "user",
    "moderation",
    "access_programs",
    "max_tool_calls",
    "personality",
];
const COUNT_FIELDS: &[&str] = &[
    "model",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "text",
    "reasoning",
    "truncation",
    "personality",
];

/// Contains resolved input and tool bindings, never a provider credential.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PreparedRequest {
    body: Value,
    #[serde(default)]
    effects: super::controls::HarnessControlEffects,
}

impl fmt::Debug for PreparedRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRequest")
            .finish_non_exhaustive()
    }
}

impl PreparedRequest {
    pub(crate) fn from_messages(
        config: &ProviderConfig,
        messages: &[Message],
        tools: &[crate::tools::ToolDefinition],
        options: &super::GenerationOptions,
    ) -> Result<Self, String> {
        if let Some(native) = &options.native {
            native.validate_model(config)?;
            return Ok(native.clone());
        }
        Self::new(
            config,
            &legacy_input(messages)?,
            &tool_items(tools)?,
            &option_controls(options),
        )
    }

    pub(crate) fn validate_model(&self, config: &ProviderConfig) -> Result<(), String> {
        if self.body["model"].as_str() != Some(config.model.as_str())
            || self.body["store"] != false
            || self.body.get("previous_response_id").is_some()
            || self.body.get("conversation").is_some()
            || self.body.get("background").is_some()
            || config.max_output_tokens.is_some_and(|limit| {
                self.body["max_output_tokens"]
                    .as_u64()
                    .is_some_and(|value| value > u64::from(limit))
            })
        {
            return Err("prepared native context does not match the model budget".into());
        }
        super::controls::validate(config, &self.body, self.effects)
    }
    pub(crate) fn new(
        config: &ProviderConfig,
        input: &[Value],
        tools: &[Value],
        controls: &Value,
    ) -> Result<Self, String> {
        Self::new_with_effects(
            config,
            input,
            tools,
            controls,
            super::controls::HarnessControlEffects::default(),
        )
    }

    pub(crate) fn new_with_effects(
        config: &ProviderConfig,
        input: &[Value],
        tools: &[Value],
        controls: &Value,
        effects: super::controls::HarnessControlEffects,
    ) -> Result<Self, String> {
        if config.model.is_empty() || input.iter().any(|item| !item.is_object()) {
            return Err("invalid native Responses context".into());
        }
        for tool in tools {
            validate_tool(tool)?;
        }
        let mut body = json!({"model":config.model,"input":input,"tools":tools,"store":false});
        if !controls.is_null() {
            let controls = controls
                .as_object()
                .ok_or("native controls must be an object")?;
            for (key, value) in controls {
                if !CONTROL_FIELDS.contains(&key.as_str()) {
                    return Err(
                        "native controls contain a harness-owned or unsupported field".into(),
                    );
                }
                body[key] = value.clone();
            }
        }
        let requested = body
            .get("max_output_tokens")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_u64()
                    .filter(|limit| *limit > 0)
                    .ok_or("invalid native output limit")
            })
            .transpose()?;
        if let Some(limit) = config.max_output_tokens {
            if limit == 0 {
                return Err("invalid native output limit".into());
            }
            if requested.is_some_and(|requested| requested > u64::from(limit)) {
                return Err("native output limit exceeds the prepared model budget".into());
            }
            if requested.is_none() {
                body["max_output_tokens"] = json!(limit);
            }
        }
        super::controls::prepare(config, &mut body, effects)?;
        Ok(Self { body, effects })
    }

    pub(crate) fn payload(&self, stream: bool) -> Value {
        let mut body = self.body.clone();
        body["stream"] = json!(stream);
        body
    }

    /// Input counting must use the same resolved context, not a Chat projection.
    pub(crate) fn count_body(&self) -> Value {
        Value::Object(
            self.body
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| COUNT_FIELDS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    }
}

fn validate_tool(tool: &Value) -> Result<(), String> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function" | "custom") => Ok(()),
        Some("namespace") => {
            let nested = tool
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("native namespace requires tools")?;
            for tool in nested {
                if !matches!(
                    tool.get("type").and_then(Value::as_str),
                    Some("function" | "custom")
                ) {
                    return Err("native namespace contains an unresolved capability".into());
                }
            }
            Ok(())
        }
        // Hosted execution must pass through our pinned service bindings. In
        // particular a default model key must never buy implicit web search.
        _ => Err("native tools must be resolved to caller or harness bindings".into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalStatus {
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

/// Private provider-native items, including opaque reasoning continuations.
/// Public output projection and ID mapping belong to the canonical harness.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NativeTurn {
    pub provider_response_id: String,
    pub status: TerminalStatus,
    pub output: Vec<Value>,
    pub usage_details: Option<Value>,
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_service_tier: Option<String>,
    pub error: Option<Value>,
    pub incomplete_details: Option<Value>,
}

impl fmt::Debug for NativeTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeTurn")
            .field("status", &self.status)
            .field("output_items", &self.output.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct NativeCompletion {
    pub completion: Completion,
    pub turn: NativeTurn,
}

impl NativeCompletion {
    pub(crate) fn into_completion(self) -> Completion {
        let mut completion = self.completion;
        completion.native = Some(self.turn);
        completion
    }
}

/// Use this for newly accepted logical inputs/results. Provider-native output
/// items must instead remain verbatim in the private native transcript.
pub(crate) fn message_items(messages: &[Message]) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    for message in messages {
        if message.role == "tool" {
            items.push(json!({"type":"function_call_output", "call_id":message.tool_call_id.as_deref().ok_or("tool result lacks call ID")?,
                "output":native_content(&message.content, false)?}));
            continue;
        }
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant"
        ) {
            return Err("invalid native message role".into());
        }
        let assistant = message.role == "assistant";
        if !message.content.is_null() && message.content != "" {
            items.push(json!({"type":"message", "role":message.role,
                "content":native_content(&message.content, assistant)?}));
        }
        for call in &message.tool_calls {
            if call.kind != "function" {
                return Err("rich calls require their original native items".into());
            }
            items.push(json!({"type":"function_call","call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}));
        }
    }
    Ok(items)
}

fn native_content(value: &Value, assistant: bool) -> Result<Value, String> {
    if let Some(text) = value.as_str() {
        return Ok(json!([{"type":if assistant {"output_text"} else {"input_text"},"text":text}]));
    }
    let parts = value.as_array().ok_or("invalid native message content")?;
    parts
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                let mut output = part.clone();
                output["type"] = json!(if assistant {
                    "output_text"
                } else {
                    "input_text"
                });
                if output.get("text").is_none_or(|text| !text.is_string()) {
                    return Err("invalid native text part".into());
                }
                Ok(output)
            }
            Some("image_url") => {
                let url = part["image_url"]["url"]
                    .as_str()
                    .ok_or("invalid native image reference")?;
                let mut output = json!({"type":"input_image","image_url":url});
                if let Some(detail) = part["image_url"].get("detail") {
                    output["detail"] = detail.clone();
                }
                Ok(output)
            }
            Some("input_image" | "input_file" | "refusal") => Ok(part.clone()),
            _ => Err("native content requires a resolved supported part".into()),
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Value::Array)
}

fn legacy_input(messages: &[Message]) -> Result<Vec<Value>, String> {
    let mut items = message_items(messages)?;
    let mut calls = BTreeMap::<String, String>::new();
    for (position, item) in items.iter_mut().enumerate() {
        match item["type"].as_str() {
            Some("function_call") => {
                let original = item["call_id"].as_str().ok_or("call lacks ID")?.to_owned();
                let id = format!("legacy_call_{position}");
                calls.insert(original, id.clone());
                item["call_id"] = json!(id);
            }
            Some("function_call_output") => {
                let original = item["call_id"]
                    .as_str()
                    .ok_or("result lacks ID")?
                    .to_owned();
                let resolved = calls.remove(&original).ok_or("unpaired legacy result")?;
                item["call_id"] = json!(resolved);
            }
            _ => {}
        }
    }
    Ok(items)
}

pub(crate) fn tool_items(tools: &[crate::tools::ToolDefinition]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            if tool.kind != "function" {
                return Err("native tool requires a resolved binding".into());
            }
            let mut item =
                serde_json::to_value(&tool.function).map_err(|_| "invalid tool binding")?;
            item["type"] = json!("function");
            Ok(item)
        })
        .collect()
}

pub(crate) fn option_controls(options: &super::GenerationOptions) -> Value {
    let mut controls = serde_json::Map::new();
    for (name, value) in [
        ("temperature", options.temperature.map(|value| json!(value))),
        ("top_p", options.top_p.map(|value| json!(value))),
        ("tool_choice", options.tool_choice.clone()),
        (
            "parallel_tool_calls",
            options.parallel_tool_calls.map(|value| json!(value)),
        ),
        (
            "text",
            options
                .text_format
                .as_ref()
                .map(|value| json!({"format":value})),
        ),
        ("reasoning", options.reasoning.clone()),
    ] {
        if let Some(value) = value {
            controls.insert(name.into(), value);
        }
    }
    Value::Object(controls)
}

/// An upstream event is private input to the harness, not a client event.
pub(crate) struct NativeEvent {
    pub value: Value,
}

/// Only public semantic categories cross the provider/Session callback boundary.
/// Provider lifecycle, raw reasoning, and encrypted continuations never do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticKind {
    ItemAdded,
    ItemDone,
    PartAdded,
    PartDone,
    TextDelta,
    TextDone,
    AnnotationAdded,
    RefusalDelta,
    RefusalDone,
    SummaryPartAdded,
    SummaryPartDone,
    SummaryTextDelta,
    SummaryTextDone,
    ArgumentsDelta,
    ArgumentsDone,
    CustomInputDelta,
    CustomInputDone,
}

pub(crate) struct SemanticDelta {
    pub kind: SemanticKind,
    pub payload: Value,
}

impl SemanticDelta {
    pub(crate) fn event_type(&self) -> &'static str {
        match self.kind {
            SemanticKind::ItemAdded => "response.output_item.added",
            SemanticKind::ItemDone => "response.output_item.done",
            SemanticKind::PartAdded => "response.content_part.added",
            SemanticKind::PartDone => "response.content_part.done",
            SemanticKind::TextDelta => "response.output_text.delta",
            SemanticKind::TextDone => "response.output_text.done",
            SemanticKind::AnnotationAdded => "response.output_text.annotation.added",
            SemanticKind::RefusalDelta => "response.refusal.delta",
            SemanticKind::RefusalDone => "response.refusal.done",
            SemanticKind::SummaryPartAdded => "response.reasoning_summary_part.added",
            SemanticKind::SummaryPartDone => "response.reasoning_summary_part.done",
            SemanticKind::SummaryTextDelta => "response.reasoning_summary_text.delta",
            SemanticKind::SummaryTextDone => "response.reasoning_summary_text.done",
            SemanticKind::ArgumentsDelta => "response.function_call_arguments.delta",
            SemanticKind::ArgumentsDone => "response.function_call_arguments.done",
            SemanticKind::CustomInputDelta => "response.custom_tool_call_input.delta",
            SemanticKind::CustomInputDone => "response.custom_tool_call_input.done",
        }
    }

    pub(crate) fn from_event(event: &Value) -> Result<Option<Self>, String> {
        let kind = match event["type"].as_str().unwrap_or("") {
            "response.output_item.added" => SemanticKind::ItemAdded,
            "response.output_item.done" => SemanticKind::ItemDone,
            "response.content_part.added" => SemanticKind::PartAdded,
            "response.content_part.done" => SemanticKind::PartDone,
            "response.output_text.delta" => SemanticKind::TextDelta,
            "response.output_text.done" => SemanticKind::TextDone,
            "response.output_text.annotation.added" => SemanticKind::AnnotationAdded,
            "response.refusal.delta" => SemanticKind::RefusalDelta,
            "response.refusal.done" => SemanticKind::RefusalDone,
            "response.reasoning_summary_part.added" => SemanticKind::SummaryPartAdded,
            "response.reasoning_summary_part.done" => SemanticKind::SummaryPartDone,
            "response.reasoning_summary_text.delta" => SemanticKind::SummaryTextDelta,
            "response.reasoning_summary_text.done" => SemanticKind::SummaryTextDone,
            "response.function_call_arguments.delta" => SemanticKind::ArgumentsDelta,
            "response.function_call_arguments.done" => SemanticKind::ArgumentsDone,
            "response.custom_tool_call_input.delta" => SemanticKind::CustomInputDelta,
            "response.custom_tool_call_input.done" => SemanticKind::CustomInputDone,
            _ => return Ok(None),
        };
        index(event, "output_index")?;
        let mut payload = select_fields(
            event,
            &[
                "output_index",
                "item_id",
                "content_index",
                "summary_index",
                "delta",
                "text",
                "refusal",
                "arguments",
                "input",
                "name",
                "logprobs",
                "annotation_index",
                "annotation",
            ],
        );
        if let Some(annotation) = event.get("annotation") {
            let Some(annotation) = public_annotation(annotation) else {
                return Ok(None);
            };
            payload["annotation"] = annotation;
        }
        if let Some(logprobs) = event.get("logprobs") {
            payload["logprobs"] = public_logprobs(logprobs);
        }
        if matches!(kind, SemanticKind::ItemAdded | SemanticKind::ItemDone) {
            let Some(item) = public_item(&event["item"])? else {
                return Ok(None);
            };
            payload["item"] = item;
        }
        if matches!(
            kind,
            SemanticKind::PartAdded
                | SemanticKind::PartDone
                | SemanticKind::SummaryPartAdded
                | SemanticKind::SummaryPartDone
        ) {
            let Some(part) = public_part(&event["part"]) else {
                return Ok(None);
            };
            payload["part"] = part;
        }
        Ok(Some(Self { kind, payload }))
    }
}

fn select_fields(value: &Value, names: &[&str]) -> Value {
    Value::Object(
        names
            .iter()
            .filter_map(|name| {
                value
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect(),
    )
}

fn public_part(value: &Value) -> Option<Value> {
    match value["type"].as_str()? {
        "output_text" => {
            let mut part = select_fields(value, &["type", "text"]);
            if let Some(annotations) = value.get("annotations").and_then(Value::as_array) {
                part["annotations"] =
                    Value::Array(annotations.iter().filter_map(public_annotation).collect());
            }
            if let Some(logprobs) = value.get("logprobs") {
                part["logprobs"] = public_logprobs(logprobs);
            }
            Some(part)
        }
        "refusal" => Some(select_fields(value, &["type", "refusal"])),
        "summary_text" => Some(select_fields(value, &["type", "text"])),
        _ => None,
    }
}

fn public_annotation(value: &Value) -> Option<Value> {
    let fields: &[&str] = match value["type"].as_str()? {
        "url_citation" => &["type", "start_index", "end_index", "url", "title"],
        "file_citation" => &["type", "file_id", "filename", "index"],
        "container_file_citation" => &[
            "type",
            "container_id",
            "file_id",
            "filename",
            "start_index",
            "end_index",
        ],
        "file_path" => &["type", "file_id", "index"],
        _ => return None,
    };
    Some(select_fields(value, fields))
}

fn public_logprobs(value: &Value) -> Value {
    Value::Array(
        value
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| {
                let mut token = select_fields(value, &["token", "logprob", "bytes"]);
                if let Some(top) = value.get("top_logprobs").and_then(Value::as_array) {
                    token["top_logprobs"] = Value::Array(
                        top.iter()
                            .map(|value| select_fields(value, &["token", "logprob", "bytes"]))
                            .collect(),
                    );
                }
                token
            })
            .collect(),
    )
}

pub(crate) fn public_item(value: &Value) -> Result<Option<Value>, String> {
    nonempty(value, "id")?;
    let fields: &[&str] = match value["type"].as_str().unwrap_or("") {
        "message" => &["id", "type", "status", "role", "phase"],
        "reasoning" => &["id", "type", "status"],
        "function_call" => &[
            "id",
            "type",
            "status",
            "call_id",
            "name",
            "namespace",
            "arguments",
            "async",
        ],
        "custom_tool_call" => &[
            "id",
            "type",
            "status",
            "call_id",
            "name",
            "namespace",
            "input",
            "async",
        ],
        // Hosted tools have a separate service authority and public projection.
        _ => return Ok(None),
    };
    let mut item = select_fields(value, fields);
    if matches!(
        value["type"].as_str(),
        Some("function_call" | "custom_tool_call")
    ) {
        if let Some(caller) = value.get("caller") {
            item["caller"] = select_fields(caller, &["type", "caller_id"]);
        }
    }
    for field in ["content", "summary"] {
        if field == "content" && value["type"] != "message" {
            continue;
        }
        if let Some(parts) = value.get(field).and_then(Value::as_array) {
            item[field] = Value::Array(parts.iter().filter_map(public_part).collect());
        }
    }
    Ok(Some(item))
}

pub(crate) async fn complete(
    config: &ProviderConfig,
    prepared: &PreparedRequest,
) -> worker::Result<NativeCompletion> {
    let trace = config.trace_context();
    let _span = crate::telemetry::NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, prepared.payload(false)).await?;
    let value = read_json(&mut response).await?;
    parse(value).map_err(|_| protocol_failure())
}

pub(crate) async fn complete_stream<F: FnMut(&NativeEvent)>(
    config: &ProviderConfig,
    prepared: &PreparedRequest,
    mut on_event: F,
) -> worker::Result<NativeCompletion> {
    let trace = config.trace_context();
    let _span = crate::telemetry::NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, prepared.payload(true)).await?;
    let mut bytes = 0usize;
    let mut events = response
        .stream()?
        .map(move |chunk| {
            let chunk = chunk?;
            bytes = bytes.saturating_add(chunk.len());
            if bytes > MAX_BYTES {
                return Err(protocol_failure());
            }
            Ok(chunk)
        })
        .eventsource();
    let mut accumulator = Accumulator::default();
    while let Some(event) = events.next().await {
        let event = event.map_err(|_| protocol_failure())?;
        // Native Responses terminates semantically; [DONE] is not required.
        if event.data.trim() == "[DONE]" {
            return Err(protocol_failure());
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| protocol_failure())?;
        if !event.event.is_empty()
            && event.event != "message"
            && value.get("type").and_then(Value::as_str) != Some(event.event.as_str())
        {
            return Err(protocol_failure());
        }
        let terminal = accumulator.push(&value).map_err(|_| protocol_failure())?;
        if accumulator.semantic_valid() {
            on_event(&NativeEvent { value });
        }
        if let Some(completion) = terminal {
            return Ok(completion);
        }
    }
    Err(protocol_failure())
}

async fn send(config: &ProviderConfig, body: Value) -> worker::Result<worker::Response> {
    if config.protocol == agents_core::ProviderProtocol::Anthropic {
        return Err(protocol_failure());
    }
    let base = crate::auth::provider_keys::normalize_base_url(config.protocol, &config.base_url)?;
    if config.api_key.is_empty()
        || config.model.is_empty()
        || body.get("model").and_then(Value::as_str) != Some(config.model.as_str())
    {
        return Err(protocol_failure());
    }
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("Authorization", &format!("Bearer {}", config.api_key))?;
    if let Some(trace) = config.trace_context() {
        trace.inject(&headers)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_redirect(RequestRedirect::Manual)
        .with_body(Some(body.to_string().into()));
    let request =
        Request::new_with_init(&format!("{}/responses", base.trim_end_matches('/')), &init)?;
    let response = Fetch::Request(request).send().await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(worker::Error::RustError(format!(
            "provider returned HTTP {}",
            response.status_code()
        )));
    }
    Ok(response)
}

async fn read_json(response: &mut worker::Response) -> worker::Result<Value> {
    let mut source = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk?;
        if chunk.len() > MAX_BYTES.saturating_sub(bytes.len()) {
            return Err(protocol_failure());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| protocol_failure())
}

fn protocol_failure() -> worker::Error {
    worker::Error::RustError("provider_protocol_error".into())
}

pub(crate) fn parse(value: Value) -> Result<NativeCompletion, String> {
    if value.get("object").and_then(Value::as_str) != Some("response") {
        return Err("invalid native response object".into());
    }
    let id = nonempty(&value, "id")?.to_owned();
    let model = nonempty(&value, "model")?.to_owned();
    let status: TerminalStatus = serde_json::from_value(value["status"].clone())
        .map_err(|_| "native response is not terminal")?;
    let output = value["output"]
        .as_array()
        .ok_or("native response lacks output")?
        .clone();
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut call_ids = std::collections::BTreeSet::new();
    for item in &output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if item.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err("invalid native output role".into());
                }
                for part in item["content"]
                    .as_array()
                    .ok_or("invalid native message content")?
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(part["text"].as_str().ok_or("invalid native output text")?);
                    }
                }
            }
            Some("function_call") => {
                let call_id = nonempty(item, "call_id")?.to_owned();
                if !call_ids.insert(call_id.clone()) {
                    return Err("duplicate native call ID".into());
                }
                calls.push(ToolCall {
                    id: call_id,
                    kind: "function".into(),
                    function: FunctionCall {
                        name: nonempty(item, "name")?.into(),
                        arguments: item["arguments"]
                            .as_str()
                            .ok_or("invalid native function arguments")?
                            .into(),
                    },
                });
            }
            Some(_) => {}
            None => return Err("native output item lacks type".into()),
        }
    }
    let usage_details = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .cloned();
    let usage = normalize_usage(&id, usage_details.as_ref())?;
    let reason = match status {
        TerminalStatus::Completed if !calls.is_empty() => "tool_calls",
        TerminalStatus::Completed => "stop",
        TerminalStatus::Incomplete
            if value["incomplete_details"]["reason"] == "max_output_tokens" =>
        {
            "length"
        }
        TerminalStatus::Incomplete if value["incomplete_details"]["reason"] == "content_filter" => {
            "content_filter"
        }
        _ => "error",
    };
    let turn = NativeTurn {
        provider_response_id: id.clone(),
        status,
        output,
        effective_service_tier: value
            .get("service_tier")
            .and_then(Value::as_str)
            .filter(|tier| {
                matches!(
                    *tier,
                    "auto" | "default" | "flex" | "scale" | "priority" | "fast" | "ultrafast"
                )
            })
            .map(str::to_owned),
        cache_write_tokens: usage_details
            .as_ref()
            .and_then(|usage| usage["input_tokens_details"]["cache_write_tokens"].as_u64()),
        usage_details,
        error: value.get("error").filter(|error| !error.is_null()).cloned(),
        incomplete_details: value
            .get("incomplete_details")
            .filter(|details| !details.is_null())
            .cloned(),
    };
    Ok(NativeCompletion {
        completion: Completion {
            id,
            model,
            message: Message {
                role: "assistant".into(),
                content: Value::String(text),
                tool_calls: calls,
                tool_call_id: None,
                extra: BTreeMap::new(),
            },
            finish_reason: reason.into(),
            usage,
            native: None,
        },
        turn,
    })
}

fn nonempty<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("native response lacks {name}"))
}

fn normalize_usage(id: &str, value: Option<&Value>) -> Result<Usage, String> {
    let input = value.and_then(|value| value["input_tokens"].as_u64());
    let output = value.and_then(|value| value["output_tokens"].as_u64());
    let total = input
        .zip(output)
        .and_then(|(input, output)| input.checked_add(output));
    let declared = value.and_then(|value| value["total_tokens"].as_u64());
    if declared
        .zip(total)
        .is_some_and(|(declared, total)| declared != total)
    {
        return Err("native usage totals disagree".into());
    }
    let cached = value.and_then(|value| value["input_tokens_details"]["cached_tokens"].as_u64());
    let reasoning =
        value.and_then(|value| value["output_tokens_details"]["reasoning_tokens"].as_u64());
    if cached
        .zip(input)
        .is_some_and(|(cached, input)| cached > input)
        || reasoning
            .zip(output)
            .is_some_and(|(reasoning, output)| reasoning > output)
    {
        return Err("native usage details exceed their reported totals".into());
    }
    Ok(Usage {
        prompt_tokens: input.unwrap_or(0),
        completion_tokens: output.unwrap_or(0),
        total_tokens: declared.or(total).unwrap_or(0),
        cached_tokens: cached.unwrap_or(0),
        reasoning_tokens: reasoning,
        cache_write_tokens: value
            .and_then(|value| value["input_tokens_details"]["cache_write_tokens"].as_u64()),
        provider_request_id: Some(id.into()),
        // The compatibility Usage scalar cannot represent an unknown cache
        // breakdown. Keep it unreported until native billing/projection uses
        // the original detail fields instead of pricing a fabricated miss.
        reported: input.is_some() && output.is_some() && cached.is_some(),
        cost: value
            .and_then(|value| value.get("cost"))
            .filter(|cost| !cost.is_null())
            .cloned(),
    })
}

#[derive(Default)]
pub(crate) struct Accumulator {
    id: Option<String>,
    sequence: Option<u64>,
    text: BTreeMap<(usize, usize), String>,
    fragments: BTreeMap<(usize, usize, &'static str), String>,
    item_ids: BTreeMap<usize, String>,
    integrity_error: bool,
    terminal: bool,
}

impl Accumulator {
    pub(crate) fn semantic_valid(&self) -> bool {
        !self.integrity_error
    }

    pub(crate) fn push(&mut self, value: &Value) -> Result<Option<NativeCompletion>, String> {
        if self.terminal {
            return Err("native event after terminal".into());
        }
        let kind = value["type"].as_str().ok_or("native event lacks type")?;
        let sequence = value["sequence_number"]
            .as_u64()
            .ok_or("native event lacks sequence")?;
        if self.sequence.is_some_and(|previous| sequence <= previous) {
            return Err("native event sequence did not advance".into());
        }
        self.sequence = Some(sequence);
        if kind == "response.created" {
            if self.id.is_some() {
                return Err("duplicate native creation event".into());
            }
            self.id = Some(nonempty(&value["response"], "id")?.into());
        }
        let id = self
            .id
            .as_deref()
            .ok_or("native stream did not begin with creation")?;
        if let Some(response) = value.get("response") {
            if response.get("id").and_then(Value::as_str) != Some(id) {
                return Err("native stream response identity changed".into());
            }
        }
        if matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) {
            let output = index(value, "output_index")?;
            let item_id = nonempty(&value["item"], "id")?;
            if self.item_ids.get(&output).is_some_and(|id| id != item_id) {
                self.integrity_error = true;
            }
            self.item_ids.insert(output, item_id.into());
        }
        if let Some(item_id) = value.get("item_id").and_then(Value::as_str) {
            let output = index(value, "output_index")?;
            if self.item_ids.get(&output).is_some_and(|id| id != item_id) {
                self.integrity_error = true;
            }
        }
        let fragment = match kind {
            "response.refusal.delta" => Some(("refusal", index(value, "content_index")?)),
            "response.reasoning_summary_text.delta" => {
                Some(("summary", index(value, "summary_index")?))
            }
            "response.function_call_arguments.delta" => Some(("arguments", 0)),
            "response.custom_tool_call_input.delta" => Some(("input", 0)),
            _ => None,
        };
        if let Some((field, part)) = fragment {
            self.fragments
                .entry((index(value, "output_index")?, part, field))
                .or_default()
                .push_str(
                    value["delta"]
                        .as_str()
                        .ok_or("invalid native semantic delta")?,
                );
        }
        let done = match kind {
            "response.refusal.done" => Some(("refusal", index(value, "content_index")?, "refusal")),
            "response.reasoning_summary_text.done" => {
                Some(("summary", index(value, "summary_index")?, "text"))
            }
            "response.function_call_arguments.done" => Some(("arguments", 0, "arguments")),
            "response.custom_tool_call_input.done" => Some(("input", 0, "input")),
            _ => None,
        };
        if let Some((field, part, source)) = done {
            let key = (index(value, "output_index")?, part, field);
            let text = value[source]
                .as_str()
                .ok_or("invalid native semantic done")?;
            if self
                .fragments
                .get(&key)
                .is_some_and(|fragment| fragment != text)
            {
                self.integrity_error = true;
            }
            self.fragments.insert(key, text.into());
        }
        if kind == "response.output_text.done" {
            let key = (
                index(value, "output_index")?,
                index(value, "content_index")?,
            );
            let text = value["text"].as_str().ok_or("invalid native text done")?;
            if self.text.get(&key).is_some_and(|fragment| fragment != text) {
                self.integrity_error = true;
            }
            self.text.insert(key, text.into());
        }
        if kind == "response.output_text.delta" {
            let output = index(value, "output_index")?;
            let content = index(value, "content_index")?;
            self.text
                .entry((output, content))
                .or_default()
                .push_str(value["delta"].as_str().ok_or("invalid native text delta")?);
        }
        if !matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed" | "response.cancelled"
        ) {
            if kind == "error" {
                return Err("upstream native stream error".into());
            }
            return Ok(None);
        }
        let mut parsed = parse(value["response"].clone())?;
        if kind
            != format!(
                "response.{}",
                value["response"]["status"].as_str().unwrap_or("")
            )
        {
            self.integrity_error = true;
        }
        for (output, id) in &self.item_ids {
            if parsed
                .turn
                .output
                .get(*output)
                .and_then(|item| item["id"].as_str())
                != Some(id.as_str())
            {
                self.integrity_error = true;
            }
        }
        for ((output, part, field), fragment) in &self.fragments {
            let item = parsed.turn.output.get(*output);
            let actual = match *field {
                "arguments" | "input" => item
                    .and_then(|item| item.get(*field))
                    .and_then(Value::as_str),
                "summary" => item
                    .and_then(|item| item["summary"].get(*part))
                    .and_then(|part| part["text"].as_str()),
                "refusal" => item
                    .and_then(|item| item["content"].get(*part))
                    .and_then(|part| part["refusal"].as_str()),
                _ => None,
            };
            if actual != Some(fragment.as_str()) {
                self.integrity_error = true;
            }
        }
        for ((output, content), text) in &self.text {
            let actual = parsed
                .turn
                .output
                .get(*output)
                .and_then(|item| item.get("content"))
                .and_then(Value::as_array)
                .and_then(|parts| parts.get(*content))
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str);
            if actual != Some(text.as_str()) {
                self.integrity_error = true;
            }
        }
        if self.integrity_error {
            // Preserve known usage for the Session accounting barrier. A
            // malformed semantic stream is failed, never a successful turn.
            parsed.turn.status = TerminalStatus::Failed;
            parsed.turn.error = Some(
                json!({"code":"provider_protocol_error","message":"Provider stream disagrees with its terminal output"}),
            );
            parsed.completion.finish_reason = "error".into();
        }
        self.terminal = true;
        Ok(Some(parsed))
    }
}

fn index(value: &Value, name: &str) -> Result<usize, String> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("invalid native {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_tool_and_summary_fragments_must_match_done_and_terminal() {
        let mut parser = Accumulator::default();
        parser.push(&json!({"type":"response.created","sequence_number":0,"response":{"id":"upstream-response"}})).unwrap();
        parser
            .push(
                &json!({"type":"response.function_call_arguments.delta","sequence_number":1,
            "output_index":2,"item_id":"call","delta":"{\"unexpected\":true}"}),
            )
            .unwrap();
        assert!(
            parser
                .push(
                    &json!({"type":"response.function_call_arguments.done","sequence_number":2,
            "output_index":2,"item_id":"call","arguments":"{}"})
                )
                .unwrap()
                .is_none()
        );
        assert!(!parser.semantic_valid());
        let mut parser = Accumulator::default();
        parser.push(&json!({"type":"response.created","sequence_number":0,"response":{"id":"upstream-response"}})).unwrap();
        parser
            .push(
                &json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,
            "output_index":0,"item_id":"reasoning","summary_index":0,"delta":"Visible summary"}),
            )
            .unwrap();
        let failed = parser
            .push(&json!({"type":"response.completed","sequence_number":2,
            "response":response("completed")}))
            .unwrap()
            .unwrap();
        assert_eq!(failed.turn.status, TerminalStatus::Failed);
        assert!(failed.completion.usage.reported);
    }

    #[test]
    fn incremental_semantics_strip_private_state_without_waiting_for_terminal() {
        let mut parser = Accumulator::default();
        let events = [
            json!({"type":"response.created","sequence_number":0,"response":{"id":"r"}}),
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,
                "item":{"id":"reason","type":"reasoning","summary":[],"content":[{"text":"private"}],"encrypted_content":"secret","created_by":"upstream"}}),
            json!({"type":"response.reasoning_summary_text.delta","sequence_number":2,
                "output_index":0,"item_id":"reason","summary_index":0,"delta":"Public summary","encrypted_content":"secret"}),
            json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,
                "item":{"id":"m","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","sequence_number":4,"output_index":1,
                "item_id":"m","content_index":0,"delta":"First token"}),
        ];
        let mut visible = Vec::new();
        for event in events {
            assert!(parser.push(&event).unwrap().is_none());
            if let Some(delta) = SemanticDelta::from_event(&event).unwrap() {
                assert!(!delta.payload.to_string().contains("secret"));
                assert!(delta.payload.get("sequence_number").is_none());
                visible.push((delta.event_type(), delta.payload));
            }
        }
        assert_eq!(visible.len(), 4);
        assert_eq!(
            visible[0].1["item"],
            json!({"id":"reason","type":"reasoning","summary":[]})
        );
        assert_eq!(visible[3].0, "response.output_text.delta");
        assert_eq!(visible[3].1["delta"], "First token");
        assert!(!parser.terminal);
        assert!(
            SemanticDelta::from_event(&json!({"type":"response.reasoning_text.delta",
            "output_index":0,"delta":"private reasoning"}))
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn semantic_payloads_cover_public_tool_and_part_events_only() {
        for kind in [
            "response.content_part.added",
            "response.content_part.done",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_part.done",
        ] {
            let event = json!({"type":kind,"output_index":0,"item_id":"i","content_index":0,
                "summary_index":0,"part":{"type":"summary_text","text":"safe","private":"secret"}});
            let delta = SemanticDelta::from_event(&event).unwrap().unwrap();
            assert_eq!(delta.event_type(), kind);
            assert_eq!(
                delta.payload["part"],
                json!({"type":"summary_text","text":"safe"})
            );
        }
        for (kind, field) in [
            ("response.function_call_arguments.delta", "delta"),
            ("response.function_call_arguments.done", "arguments"),
            ("response.custom_tool_call_input.delta", "delta"),
            ("response.custom_tool_call_input.done", "input"),
            ("response.refusal.delta", "delta"),
            ("response.refusal.done", "refusal"),
        ] {
            let mut event = json!({"type":kind,"output_index":1,"item_id":"i"});
            event[field] = json!("fragment");
            let delta = SemanticDelta::from_event(&event).unwrap().unwrap();
            assert_eq!(delta.event_type(), kind);
            assert_eq!(delta.payload[field], "fragment");
        }
        assert!(
            SemanticDelta::from_event(
                &json!({"type":"response.output_item.added", "output_index":0,
            "item":{"id":"host","type":"mcp_call","authorization":"secret"}})
            )
            .unwrap()
            .is_none()
        );
    }

    fn config() -> ProviderConfig {
        ProviderConfig {
            protocol: agents_core::ProviderProtocol::Openai,
            base_url: "https://provider.example/v1".into(),
            api_key: "test-only".into(),
            model: "model".into(),
            max_output_tokens: Some(256),
            session_id: None,
            traceparent: None,
        }
    }

    fn response(status: &str) -> Value {
        json!({"id":"upstream-response","object":"response","model":"resolved-model","status":status,
            "output":[{"id":"reasoning","type":"reasoning","encrypted_content":"opaque-provider-state"},
                {"id":"message","type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"Hello 雪","annotations":[{"type":"url_citation","url":"https://example.com","title":"Source","start_index":0,"end_index":5}]}]},
                {"id":"call","type":"function_call","call_id":"call-1","name":"bound_function","arguments":"{}"}],
            "usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens":3,
                "output_tokens_details":{"reasoning_tokens":2},"total_tokens":15,"cost":0.00000037},
            "error":null,"incomplete_details":null})
    }

    #[test]
    fn prepared_context_is_shared_with_counting_without_provider_state() {
        let input = vec![
            json!({"role":"system","content":"Pinned server and skills"}),
            json!({"role":"developer","content":"Current custom instructions"}),
            json!({"type":"reasoning","encrypted_content":"opaque"}),
        ];
        let tools = vec![
            json!({"type":"function","name":"__agents_code_interpreter","parameters":{"type":"object"}}),
        ];
        let prepared = PreparedRequest::new(&config(), &input, &tools,
            &json!({"temperature":0.4,"tool_choice":"auto","text":{"format":{"type":"text"}},"reasoning":{"effort":"high"}})).unwrap();
        let body = prepared.payload(true);
        let count = prepared.count_body();
        assert_eq!(body["input"], count["input"]);
        assert_eq!(body["tools"], count["tools"]);
        assert_eq!(body["model"], "model");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(count["reasoning"], body["reasoning"]);
        for field in [
            "store",
            "stream",
            "background",
            "max_output_tokens",
            "temperature",
            "previous_response_id",
            "conversation",
        ] {
            assert!(count.get(field).is_none(), "{field}");
        }
        for controls in [
            json!({"previous_response_id":"foreign"}),
            json!({"store":true}),
            json!({"model":"other"}),
            json!({"max_output_tokens":257}),
        ] {
            assert!(PreparedRequest::new(&config(), &input, &tools, &controls).is_err());
        }
        for kind in [
            "web_search",
            "code_interpreter",
            "image_generation",
            "mcp",
            "unknown",
        ] {
            assert!(
                PreparedRequest::new(&config(), &input, &[json!({"type":kind})], &Value::Null)
                    .is_err()
            );
        }
    }

    #[test]
    fn explicit_native_context_preserves_opaque_items_and_legacy_calls_remain_paired() {
        let opaque = vec![json!({"type":"reasoning","encrypted_content":"private-context"})];
        let prepared = PreparedRequest::new(&config(), &opaque, &[], &Value::Null).unwrap();
        let options = super::super::GenerationOptions {
            native: Some(prepared),
            ..Default::default()
        };
        let rendered = PreparedRequest::from_messages(
            &config(),
            &[Message::text("user", "not the selected context")],
            &[],
            &options,
        )
        .unwrap();
        assert_eq!(rendered.payload(false)["input"], Value::Array(opaque));
        let call = || ToolCall {
            id: "call_0".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "tool".into(),
                arguments: "{}".into(),
            },
        };
        let assistant = || Message {
            role: "assistant".into(),
            tool_calls: vec![call()],
            ..Default::default()
        };
        let messages = vec![
            assistant(),
            Message::tool_result("call_0", "First"),
            assistant(),
            Message::tool_result("call_0", "Second"),
        ];
        let input = legacy_input(&messages).unwrap();
        assert_eq!(input[0]["call_id"], input[1]["call_id"]);
        assert_eq!(input[2]["call_id"], input[3]["call_id"]);
        assert_ne!(input[0]["call_id"], input[2]["call_id"]);
        assert_eq!(
            message_items(&messages[1..2]).unwrap()[0]["call_id"],
            "call_0"
        );
    }

    #[test]
    fn normalized_message_does_not_leak_native_items_or_lose_usage() {
        let parsed = parse(response("completed")).unwrap();
        assert_eq!(parsed.completion.message.content, "Hello 雪");
        assert_eq!(parsed.completion.message.tool_calls.len(), 1);
        assert!(parsed.completion.message.extra.is_empty());
        assert_eq!(
            parsed.turn.output[0]["encrypted_content"],
            "opaque-provider-state"
        );
        assert_eq!(
            parsed.turn.output[1]["content"][0]["annotations"][0]["type"],
            "url_citation"
        );
        assert_eq!(parsed.completion.usage.prompt_tokens, 12);
        assert_eq!(parsed.completion.usage.reasoning_tokens, Some(2));
        assert_eq!(parsed.turn.cache_write_tokens, None);
        let mut partial = response("completed");
        partial["usage"]["input_tokens_details"] = json!({});
        let partial = parse(partial).unwrap();
        assert!(!partial.completion.usage.reported);
        assert_eq!(partial.turn.usage_details.unwrap()["input_tokens"], 12);
        assert!(parsed.completion.usage.cost.is_some());
        assert!(!format!("{:?}", parsed.turn).contains("opaque-provider-state"));
    }

    #[test]
    fn known_failed_usage_is_preserved_without_turning_failure_into_completion() {
        let mut value = response("failed");
        value["error"] = json!({"code":"server_error","message":"private provider diagnostic"});
        let parsed = parse(value).unwrap();
        assert_eq!(parsed.turn.status, TerminalStatus::Failed);
        assert_eq!(parsed.completion.finish_reason, "error");
        assert!(parsed.completion.usage.reported);
        let mut absent = response("completed");
        absent.as_object_mut().unwrap().remove("usage");
        let parsed = parse(absent).unwrap();
        assert!(!parsed.completion.usage.reported);
        assert!(parsed.turn.usage_details.is_none());
        assert_eq!(parsed.turn.cache_write_tokens, None);
    }

    #[test]
    fn semantic_terminal_ends_stream_without_done_and_rejects_changed_identity_or_text() {
        let mut accumulator = Accumulator::default();
        assert!(accumulator.push(&json!({"type":"response.created","sequence_number":0,"response":{"id":"upstream-response"}})).unwrap().is_none());
        assert!(accumulator.push(&json!({"type":"response.output_text.delta","sequence_number":3,"output_index":1,"content_index":0,"delta":"Hello 雪"})).unwrap().is_none());
        let terminal = json!({"type":"response.completed","sequence_number":5,"response":response("completed")});
        assert!(accumulator.push(&terminal).unwrap().is_some());
        assert!(accumulator.push(&terminal).is_err());
        let mut mismatch = Accumulator::default();
        mismatch
            .push(
                &json!({"type":"response.created","sequence_number":0,"response":{"id":"another"}}),
            )
            .unwrap();
        assert!(mismatch.push(&terminal).is_err());
        let mut mismatch = Accumulator::default();
        mismatch.push(&json!({"type":"response.created","sequence_number":0,"response":{"id":"upstream-response"}})).unwrap();
        mismatch.push(&json!({"type":"response.output_text.delta","sequence_number":1,"output_index":1,"content_index":0,"delta":"Wrong"})).unwrap();
        let failed = mismatch.push(&terminal).unwrap().unwrap();
        assert_eq!(failed.turn.status, TerminalStatus::Failed);
        assert!(failed.completion.usage.reported);
    }
}
