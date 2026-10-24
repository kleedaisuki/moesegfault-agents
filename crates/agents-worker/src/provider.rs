//! Provider continuation contracts:
//! - <https://api-docs.deepseek.com/guides/tool_calls/>
//! - <https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>
//! Reasoning continuation fields belong to persisted messages, not telemetry.

use std::collections::BTreeMap;

use agents_core::ProviderProtocol;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

use crate::{
    telemetry::{NativeSpan, TraceContext},
    tools::ToolDefinition,
};

#[path = "provider_anthropic.rs"]
mod anthropic;

const MAX_COMPLETION_BYTES: usize = 16 * 1024 * 1024;

/// Per-response generation policy, separate from the session's provider secrets.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub tool_choice: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    pub text_format: Option<Value>,
    pub reasoning: Option<Value>,
}

impl GenerationOptions {
    // DeepSeek accepts some fields while ignoring them in particular modes.
    // Reject those combinations rather than misrepresenting the user's policy.
    // https://api-docs.deepseek.com/guides/thinking_mode/
    // https://api-docs.deepseek.com/guides/anthropic_api/
    pub fn validate_for_config(
        &self,
        config: &ProviderConfig,
        tools: &[ToolDefinition],
    ) -> Result<(), String> {
        if !is_deepseek(config) {
            return self.validate(config.protocol, tools);
        }
        self.validate(ProviderProtocol::Openai, tools)?;
        if self.text_format.as_ref().is_some_and(|format| {
            format["type"] == "json_schema"
                || (config.protocol == ProviderProtocol::Anthropic && format["type"] != "text")
        }) {
            return Err(
                "This DeepSeek endpoint does not support the requested output format".into(),
            );
        }
        if config.protocol == ProviderProtocol::Anthropic
            && (self.parallel_tool_calls == Some(false)
                || tools.iter().any(|tool| tool.function.strict == Some(true)))
        {
            return Err(
                "The DeepSeek Anthropic endpoint does not enforce strict or single-tool generation"
                    .into(),
            );
        }
        let nonthinking = self
            .reasoning
            .as_ref()
            .is_some_and(|value| value["effort"] == "none");
        if self.temperature.is_some() && !nonthinking {
            return Err("DeepSeek temperature requires reasoning.effort none".into());
        }
        if self
            .tool_choice
            .as_ref()
            .is_some_and(|choice| choice == "required" || choice.is_object())
            && !nonthinking
        {
            return Err("DeepSeek forced tool selection requires reasoning.effort none".into());
        }
        if let Some(top_p) = self.top_p {
            let supported = if config.model.contains("flash") {
                !nonthinking && top_p >= 0.95
            } else if config.model.contains("pro") {
                nonthinking
            } else {
                false
            };
            if !supported {
                return Err("This DeepSeek mode does not honor the requested top_p".into());
            }
        }
        Ok(())
    }

    pub fn validate(
        &self,
        protocol: ProviderProtocol,
        tools: &[ToolDefinition],
    ) -> Result<(), String> {
        if self
            .temperature
            .is_some_and(|v| !v.is_finite() || !(0.0..=2.0).contains(&v))
        {
            return Err("temperature must be between 0 and 2".into());
        }
        if self
            .top_p
            .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err("top_p must be between 0 and 1".into());
        }
        if protocol == ProviderProtocol::Anthropic && self.temperature.is_some_and(|v| v > 1.0) {
            return Err("Anthropic temperature must be between 0 and 1".into());
        }
        if let Some(choice) = &self.tool_choice {
            validate_tool_choice(choice, tools)?;
        }
        if let Some(format) = &self.text_format {
            validate_text_format(format)?;
            if protocol == ProviderProtocol::Anthropic && format["type"] == "json_object" {
                return Err(
                    "Anthropic does not support json_object output; use json_schema".into(),
                );
            }
        }
        if let Some(reasoning) = &self.reasoning {
            validate_reasoning(reasoning)?;
            if protocol == ProviderProtocol::Anthropic {
                return Err(
                    "reasoning overrides are not supported by the Anthropic adapter".into(),
                );
            }
        }
        Ok(())
    }
}

fn provider_host(config: &ProviderConfig) -> Option<String> {
    worker::Url::parse(&config.base_url)
        .ok()?
        .host_str()
        .map(str::to_owned)
}

pub(super) fn is_deepseek(config: &ProviderConfig) -> bool {
    provider_host(config).as_deref() == Some("api.deepseek.com")
}

pub(super) fn deepseek_effort(effort: &str) -> &str {
    match effort {
        "minimal" => "low",
        "medium" | "xhigh" => "high",
        other => other,
    }
}

fn validate_tool_choice(choice: &Value, tools: &[ToolDefinition]) -> Result<(), String> {
    match choice.as_str() {
        Some("none" | "auto") => return Ok(()),
        Some("required") if !tools.is_empty() => return Ok(()),
        Some(_) => {
            return Err("tool_choice must be none, auto, required or a named function".into());
        }
        None => {}
    }
    let Some(object) = choice.as_object() else {
        return Err("tool_choice must be a string or named function".into());
    };
    if object.len() != 2 || choice["type"] != "function" {
        return Err("only named function tool_choice is supported".into());
    }
    let name = choice["name"]
        .as_str()
        .ok_or("tool_choice requires a function name")?;
    if !tools.iter().any(|tool| tool.function.name == name) {
        return Err("tool_choice names an unavailable function".into());
    }
    Ok(())
}

fn validate_text_format(format: &Value) -> Result<(), String> {
    let object = format.as_object().ok_or("text.format must be an object")?;
    match format["type"].as_str() {
        Some("text" | "json_object") if object.len() == 1 => Ok(()),
        Some("json_schema") => {
            if object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "name" | "description" | "schema" | "strict"
                )
            }) {
                return Err("unsupported text.format field".into());
            }
            let name = format["name"]
                .as_str()
                .ok_or("json_schema requires a name")?;
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            {
                return Err(
                    "json_schema name must contain 1 to 64 letters, digits, underscores or dashes"
                        .into(),
                );
            }
            if !format["schema"].is_object()
                || object
                    .get("strict")
                    .is_some_and(|v| !v.is_boolean() && !v.is_null())
                || object.get("description").is_some_and(|v| !v.is_string())
            {
                return Err("invalid json_schema output format".into());
            }
            Ok(())
        }
        _ => Err("text.format must be text, json_object or json_schema".into()),
    }
}

fn validate_reasoning(reasoning: &Value) -> Result<(), String> {
    let object = reasoning.as_object().ok_or("reasoning must be an object")?;
    if object.keys().any(|key| key != "effort") {
        return Err("only reasoning.effort is supported".into());
    }
    if object.is_empty()
        || matches!(
            reasoning["effort"].as_str(),
            Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
        )
    {
        return Ok(());
    }
    Err("invalid reasoning effort".into())
}

/// Never serialize or Debug this object: API keys must not enter session state or logs.
pub struct ProviderConfig {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: Option<u32>,
    /// Opaque session key for OpenRouter sticky cache routing; not a prompt field.
    pub session_id: Option<String>,
    /// Explicit opt-in application correlation, never credentials or trace baggage.
    pub traceparent: Option<String>,
}

impl ProviderConfig {
    fn trace_context(&self) -> Option<TraceContext> {
        self.traceparent.as_deref().and_then(TraceContext::parse)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    // Preserve reasoning_content and other provider continuation fields verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Message {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Value::String(content.into()),
            ..Self::default()
        }
    }

    pub fn tool_result(id: &str, output: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(id.into()),
            ..Self::text("tool", output)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Missing upstream usage is not the same as a free inference.
    pub reported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Completion {
    pub id: String,
    pub model: String,
    pub message: Message,
    pub finish_reason: String,
    pub usage: Usage,
}

fn endpoint(config: &ProviderConfig) -> worker::Result<String> {
    let base_url =
        crate::auth::provider_keys::normalize_base_url(config.protocol, &config.base_url)?;
    if config.api_key.is_empty() || config.model.is_empty() || config.max_output_tokens == Some(0) {
        return Err(worker::Error::RustError(
            "provider credentials, model, or output limit invalid".into(),
        ));
    }
    Ok(format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        match config.protocol {
            ProviderProtocol::Openai => "chat/completions",
            ProviderProtocol::Anthropic => "messages",
        }
    ))
}

pub async fn complete(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> worker::Result<Completion> {
    complete_with_options(config, messages, tools, &GenerationOptions::default()).await
}

pub async fn complete_with_options(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &GenerationOptions,
) -> worker::Result<Completion> {
    let trace = config.trace_context();
    let _span = NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, messages, tools, false, options).await?;
    let value = completion_json(&mut response)
        .await
        .map_err(|_| protocol_failure())?;
    match config.protocol {
        ProviderProtocol::Openai => parse_completion(value),
        ProviderProtocol::Anthropic => anthropic::parse_completion(value),
    }
    .map_err(|_| protocol_failure())
}

fn protocol_failure() -> worker::Error {
    worker::Error::RustError("provider_protocol_error".into())
}

async fn completion_json(response: &mut worker::Response) -> worker::Result<Value> {
    let mut stream = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_COMPLETION_BYTES {
            return Err(worker::Error::RustError(
                "provider completion exceeds maximum size".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| worker::Error::RustError("invalid provider completion JSON".into()))
}

async fn send(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    stream: bool,
    options: &GenerationOptions,
) -> worker::Result<worker::Response> {
    options
        .validate_for_config(config, tools)
        .map_err(worker::Error::RustError)?;
    let endpoint = endpoint(config)?;
    let body = match config.protocol {
        ProviderProtocol::Openai => {
            openai_body_with_options(config, messages, tools, stream, options)?
        }
        ProviderProtocol::Anthropic => {
            anthropic::request_body_with_options(config, messages, tools, stream, options)
                .map_err(|_| protocol_failure())?
        }
    };
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    match config.protocol {
        ProviderProtocol::Openai => {
            headers.set("Authorization", &format!("Bearer {}", config.api_key))?;
        }
        ProviderProtocol::Anthropic => {
            headers.set("x-api-key", &config.api_key)?;
            headers.set("anthropic-version", "2023-06-01")?;
        }
    }
    if let Some(trace) = config.trace_context() {
        trace.inject(&headers)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        // workerd has no "error" redirect mode. Manual returns 3xx for the
        // non-success check below without forwarding bearer credentials.
        .with_redirect(RequestRedirect::Manual)
        .with_body(Some(body.to_string().into()));
    let request = Request::new_with_init(&endpoint, &init)?;
    let response = Fetch::Request(request).send().await?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        // Provider bodies can contain credentials and user content; do not log/return them.
        return Err(worker::Error::RustError(format!(
            "provider returned HTTP {status}"
        )));
    }
    Ok(response)
}

fn openai_body_with_options(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    stream: bool,
    options: &GenerationOptions,
) -> worker::Result<Value> {
    let mut body = json!({"model":config.model,"messages":messages,"stream":stream});
    if stream {
        body["stream_options"] = json!({"include_usage":true});
    }
    if worker::Url::parse(&config.base_url)
        .ok()
        .is_some_and(|url| url.host_str() == Some("openrouter.ai"))
        && let Some(session_id) = &config.session_id
    {
        body["session_id"] = json!(session_id);
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools)?;
    }
    if let Some(limit) = config.max_output_tokens {
        body["max_tokens"] = json!(limit);
    }
    if let Some(value) = options.temperature {
        body["temperature"] = json!(value);
    }
    if let Some(value) = options.top_p {
        body["top_p"] = json!(value);
    }
    if let Some(value) = &options.tool_choice {
        body["tool_choice"] = if value.is_object() {
            json!({"type":"function","function":{"name":value["name"]}})
        } else {
            value.clone()
        };
    }
    if let Some(value) = options.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(value);
    }
    if let Some(format) = &options.text_format {
        body["response_format"] = if format["type"] == "json_schema" {
            let mut schema = format.clone();
            schema.as_object_mut().unwrap().remove("type");
            json!({"type":"json_schema","json_schema":schema})
        } else {
            format.clone()
        };
    }
    if let Some(effort) = options.reasoning.as_ref().and_then(|v| v.get("effort")) {
        if provider_host(config).as_deref() == Some("openrouter.ai") {
            body["reasoning"] = json!({"effort":effort});
        } else if is_deepseek(config) {
            let effort = deepseek_effort(effort.as_str().unwrap_or("high"));
            body["thinking"] = json!({"type":if effort == "none" {"disabled"} else {"enabled"}});
            body["reasoning_effort"] = json!(effort);
        } else {
            body["reasoning_effort"] = effort.clone();
        }
    }
    Ok(body)
}

/// Emits text as soon as the upstream sends it, while preserving full tool and
/// reasoning continuations for the next inference round. Does not buffer SSE.
pub async fn complete_stream<F: FnMut(&str)>(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    on_delta: F,
) -> worker::Result<Completion> {
    complete_stream_with_options(
        config,
        messages,
        tools,
        &GenerationOptions::default(),
        on_delta,
    )
    .await
}

pub async fn complete_stream_with_options<F: FnMut(&str)>(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &GenerationOptions,
    mut on_delta: F,
) -> worker::Result<Completion> {
    let trace = config.trace_context();
    let _span = NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, messages, tools, true, options).await?;
    let mut received = 0usize;
    let mut events = response
        .stream()
        .map_err(|_| protocol_failure())?
        .map(move |chunk| {
            let chunk = chunk?;
            received = received.saturating_add(chunk.len());
            if received > MAX_COMPLETION_BYTES {
                return Err(protocol_failure());
            }
            Ok(chunk)
        })
        .eventsource();
    if config.protocol == ProviderProtocol::Anthropic {
        let mut accumulator = anthropic::StreamAccumulator::default();
        while let Some(event) = events.next().await {
            let event = event.map_err(|_| protocol_failure())?;
            let value: Value = serde_json::from_str(&event.data).map_err(|_| protocol_failure())?;
            if let Some(text) = accumulator.push(value).map_err(|_| protocol_failure())? {
                on_delta(&text);
            }
            if accumulator.is_done() {
                break;
            }
        }
        return accumulator.finish().map_err(|_| protocol_failure());
    }
    let mut accumulator = StreamAccumulator::default();
    let mut done = false;
    while let Some(event) = events.next().await {
        let event = event.map_err(|_| protocol_failure())?;
        if event.data.trim() == "[DONE]" {
            done = true;
            break;
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| protocol_failure())?;
        if let Some(text) = accumulator.push(value).map_err(|_| protocol_failure())? {
            on_delta(&text);
        }
    }
    if !done || accumulator.finish_reason.is_none() {
        return Err(protocol_failure());
    }
    parse_completion(accumulator.finish()).map_err(|_| protocol_failure())
}

#[derive(Default)]
struct StreamAccumulator {
    id: String,
    model: String,
    content: String,
    refusal: String,
    reasoning: String,
    reasoning_text: String,
    reasoning_details: Vec<Value>,
    tools: BTreeMap<u64, Value>,
    finish_reason: Option<String>,
    usage: Value,
    bytes: usize,
}

impl StreamAccumulator {
    fn push(&mut self, value: Value) -> Result<Option<String>, String> {
        self.bytes = self.bytes.saturating_add(value.to_string().len());
        if self.bytes > MAX_COMPLETION_BYTES {
            return Err("provider completion exceeds maximum size".into());
        }
        if value.get("error").is_some() {
            return Err("provider reported a streaming error".into());
        }
        if let Some(id) = value["id"].as_str() {
            self.id = id.into();
        }
        if let Some(model) = value["model"].as_str() {
            self.model = model.into();
        }
        if value["usage"].is_object() {
            self.usage = value["usage"].clone();
        }
        let Some(choice) = value["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return Ok(None);
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.into());
        }
        let delta = &choice["delta"];
        let content = delta["content"].as_str().map(str::to_owned);
        if let Some(text) = &content {
            self.content.push_str(text);
        }
        if let Some(text) = delta["refusal"].as_str() {
            self.refusal.push_str(text);
        }
        if let Some(text) = delta["reasoning_content"].as_str() {
            self.reasoning.push_str(text);
        }
        if let Some(text) = delta["reasoning"].as_str() {
            self.reasoning_text.push_str(text);
        }
        if let Some(details) = delta["reasoning_details"].as_array() {
            // Signed/encrypted blocks must be replayed in exactly their original order.
            self.reasoning_details.extend(details.iter().cloned());
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.push_tool(call)?;
            }
        }
        Ok(content)
    }

    fn push_tool(&mut self, delta: &Value) -> Result<(), String> {
        let index = delta["index"]
            .as_u64()
            .ok_or("provider tool delta missing index")?;
        if index >= 128 {
            return Err("provider emitted too many tool calls".into());
        }
        let call = self.tools.entry(index).or_insert_with(|| {
            json!({
                "id":"","type":"function","function":{"name":"","arguments":""}
            })
        });
        for field in ["id", "type"] {
            if let Some(value) = delta[field].as_str() {
                call[field] = json!(value);
            }
        }
        for field in ["name", "arguments"] {
            if let Some(fragment) = delta["function"][field].as_str() {
                let mut value = call["function"][field]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                value.push_str(fragment);
                call["function"][field] = json!(value);
            }
        }
        Ok(())
    }

    fn finish(self) -> Value {
        let mut message = json!({"role":"assistant","content":self.content});
        if !self.refusal.is_empty() {
            message["refusal"] = json!(self.refusal);
        }
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        if !self.reasoning_text.is_empty() {
            message["reasoning"] = json!(self.reasoning_text);
        }
        if !self.reasoning_details.is_empty() {
            message["reasoning_details"] = json!(self.reasoning_details);
        }
        if !self.tools.is_empty() {
            message["tool_calls"] = json!(self.tools.into_values().collect::<Vec<_>>());
        }
        json!({"id":self.id,"model":self.model,"usage":self.usage,
            "choices":[{"message":message,"finish_reason":self.finish_reason}]})
    }
}

pub fn parse_completion(value: Value) -> Result<Completion, String> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "provider returned no completion choice".to_owned())?;
    let message: Message = serde_json::from_value(choice["message"].clone())
        .map_err(|_| "provider returned an invalid message".to_owned())?;
    if message.role != "assistant"
        || message.tool_calls.iter().any(|call| {
            call.kind != "function" || call.id.is_empty() || call.function.name.is_empty()
        })
    {
        return Err("provider returned an invalid assistant/tool call".into());
    }
    let usage_value = value.get("usage").filter(|value| value.is_object());
    let prompt_tokens = usage_value
        .and_then(|u| u["prompt_tokens"].as_u64())
        .unwrap_or(0);
    let completion_tokens = usage_value
        .and_then(|u| u["completion_tokens"].as_u64())
        .unwrap_or(0);
    let usage = Usage {
        provider_request_id: value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        prompt_tokens,
        completion_tokens,
        total_tokens: usage_value
            .and_then(|u| u["total_tokens"].as_u64())
            .unwrap_or(prompt_tokens.saturating_add(completion_tokens)),
        cached_tokens: usage_value
            .and_then(|u| {
                u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
            })
            .unwrap_or(0),
        reasoning_tokens: usage_value
            .and_then(|u| u["completion_tokens_details"]["reasoning_tokens"].as_u64()),
        reported: usage_value
            .is_some_and(|u| u["prompt_tokens"].is_u64() && u["completion_tokens"].is_u64()),
        cost: usage_value
            .and_then(|u| u.get("cost"))
            .filter(|cost| !cost.is_null())
            .cloned(),
    };
    Ok(Completion {
        id: value["id"].as_str().unwrap_or_default().into(),
        model: value["model"].as_str().unwrap_or_default().into(),
        message,
        finish_reason: choice["finish_reason"].as_str().unwrap_or("stop").into(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_multimodal_content_is_native_not_text_encoded() {
        let content = json!([
            {"type":"text","text":"Describe this."},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AQID","detail":"auto"}}
        ]);
        let message = Message {
            role: "user".into(),
            content: content.clone(),
            ..Message::default()
        };
        let body = openai_body_with_options(
            &test_config(ProviderProtocol::Openai),
            &[message],
            &[],
            false,
            &GenerationOptions::default(),
        )
        .unwrap();
        assert_eq!(body["messages"][0]["content"], content);
    }

    fn test_config(protocol: ProviderProtocol) -> ProviderConfig {
        ProviderConfig {
            protocol,
            base_url: "https://fixture.invalid/v1".into(),
            api_key: "test-only".into(),
            model: "fixture".into(),
            max_output_tokens: Some(256),
            session_id: None,
            traceparent: None,
        }
    }

    #[test]
    fn response_generation_options_translate_without_changing_defaults() {
        let config = test_config(ProviderProtocol::Openai);
        let mut tools = crate::tools::registered_tools(false);
        tools[0].function.strict = Some(true);
        let default =
            openai_body_with_options(&config, &[], &tools, false, &GenerationOptions::default())
                .unwrap();
        assert!(default.get("temperature").is_none());
        assert!(default.get("response_format").is_none());
        assert!(default.get("tool_choice").is_none());
        let options = GenerationOptions {
            temperature: Some(0.4),
            top_p: Some(0.7),
            tool_choice: Some(json!({"type":"function","name":"time"})),
            parallel_tool_calls: Some(false),
            text_format: Some(
                json!({"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}),
            ),
            reasoning: Some(json!({"effort":"low"})),
        };
        options.validate(config.protocol, &tools).unwrap();
        let body = openai_body_with_options(&config, &[], &tools, true, &options).unwrap();
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["temperature"], 0.4);
        assert_eq!(body["top_p"], 0.7);
        assert_eq!(
            body["tool_choice"],
            json!({"type":"function","function":{"name":"time"}})
        );
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(
            body["response_format"],
            json!({"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object"},"strict":true}})
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn reasoning_policy_matches_known_provider_wire_protocols() {
        let mut config = test_config(ProviderProtocol::Openai);
        config.base_url = "https://openrouter.ai/api/v1".into();
        let options = GenerationOptions {
            reasoning: Some(json!({"effort":"medium"})),
            ..GenerationOptions::default()
        };
        options.validate_for_config(&config, &[]).unwrap();
        let body = openai_body_with_options(&config, &[], &[], false, &options).unwrap();
        assert_eq!(body["reasoning"], json!({"effort":"medium"}));
        assert!(body.get("reasoning_effort").is_none());
        config.base_url = "https://api.deepseek.com/v1".into();
        options.validate_for_config(&config, &[]).unwrap();
        let body = openai_body_with_options(&config, &[], &[], false, &options).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["thinking"], json!({"type":"enabled"}));
        let options = GenerationOptions {
            reasoning: Some(json!({"effort":"none"})),
            temperature: Some(1.4),
            ..GenerationOptions::default()
        };
        options.validate_for_config(&config, &[]).unwrap();
        let body = openai_body_with_options(&config, &[], &[], false, &options).unwrap();
        assert_eq!(body["thinking"], json!({"type":"disabled"}));
    }

    #[test]
    fn known_ignored_deepseek_options_are_rejected_before_dispatch() {
        let mut config = test_config(ProviderProtocol::Openai);
        config.base_url = "https://api.deepseek.com".into();
        config.model = "deepseek-flash".into();
        for options in [
            GenerationOptions {
                temperature: Some(0.5),
                ..GenerationOptions::default()
            },
            GenerationOptions {
                top_p: Some(0.5),
                ..GenerationOptions::default()
            },
            GenerationOptions {
                text_format: Some(json!({"type":"json_schema","name":"result","schema":{}})),
                ..GenerationOptions::default()
            },
            GenerationOptions {
                tool_choice: Some(json!("required")),
                ..GenerationOptions::default()
            },
        ] {
            assert!(
                options
                    .validate_for_config(&config, &crate::tools::registered_tools(false))
                    .is_err()
            );
        }
        config.protocol = ProviderProtocol::Anthropic;
        let options = GenerationOptions {
            parallel_tool_calls: Some(false),
            ..GenerationOptions::default()
        };
        assert!(options.validate_for_config(&config, &[]).is_err());
        let options = GenerationOptions {
            reasoning: Some(json!({"effort":"none"})),
            temperature: Some(1.4),
            ..GenerationOptions::default()
        };
        assert!(options.validate_for_config(&config, &[]).is_ok());
    }

    #[test]
    fn generation_options_reject_unknown_or_unavailable_semantics() {
        let tools = crate::tools::registered_tools(false);
        for choice in [
            json!("required"),
            json!({"type":"function","name":"missing"}),
            json!({"type":"function","function":{"name":"time"}}),
        ] {
            let options = GenerationOptions {
                tool_choice: Some(choice),
                ..GenerationOptions::default()
            };
            assert!(options.validate(ProviderProtocol::Openai, &[]).is_err());
        }
        let required = GenerationOptions {
            tool_choice: Some(json!("required")),
            ..GenerationOptions::default()
        };
        assert!(required.validate(ProviderProtocol::Openai, &tools).is_ok());
        for format in [
            json!({"type":"json_schema","name":"a","schema":false}),
            json!({"type":"text","unknown":true}),
            json!({"type":"json_schema","name":"a space","schema":{}}),
        ] {
            let options = GenerationOptions {
                text_format: Some(format),
                ..GenerationOptions::default()
            };
            assert!(options.validate(ProviderProtocol::Openai, &tools).is_err());
        }
        let options = GenerationOptions {
            reasoning: Some(json!({"summary":"auto"})),
            ..GenerationOptions::default()
        };
        assert!(options.validate(ProviderProtocol::Openai, &tools).is_err());
    }

    #[test]
    fn generation_numeric_bounds_and_anthropic_capabilities_are_checked() {
        for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
            let options = GenerationOptions {
                temperature: Some(temperature),
                ..GenerationOptions::default()
            };
            assert!(options.validate(ProviderProtocol::Openai, &[]).is_err());
        }
        for top_p in [f64::NAN, -0.1, 1.1] {
            let options = GenerationOptions {
                top_p: Some(top_p),
                ..GenerationOptions::default()
            };
            assert!(options.validate(ProviderProtocol::Openai, &[]).is_err());
        }
        for options in [
            GenerationOptions {
                temperature: Some(1.1),
                ..GenerationOptions::default()
            },
            GenerationOptions {
                text_format: Some(json!({"type":"json_object"})),
                ..GenerationOptions::default()
            },
            GenerationOptions {
                reasoning: Some(json!({"effort":"low"})),
                ..GenerationOptions::default()
            },
        ] {
            assert!(options.validate(ProviderProtocol::Anthropic, &[]).is_err());
        }
    }

    #[test]
    fn provider_trace_context_is_explicit_and_validated() {
        let mut config = ProviderConfig {
            protocol: ProviderProtocol::Openai,
            base_url: "https://api.deepseek.com".into(),
            api_key: "test-only".into(),
            model: "deepseek-chat".into(),
            max_output_tokens: Some(1024),
            session_id: None,
            traceparent: None,
        };
        assert!(config.trace_context().is_none());
        config.traceparent = Some("invalid\r\nauthorization: secret".into());
        assert!(config.trace_context().is_none());
        let trace = TraceContext::from_traceparent(None).child();
        config.traceparent = Some(trace.traceparent());
        assert_eq!(
            config.trace_context().unwrap().traceparent(),
            trace.traceparent()
        );
    }

    #[test]
    fn thinking_tool_roundtrip_retains_provider_continuation() {
        let value = json!({"id":"c1","model":"deepseek-chat","choices":[{
            "message":{"role":"assistant","content":null,"reasoning_content":"reason",
                "tool_calls":[{"id":"call1","type":"function","function":{"name":"time","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":80}});
        let completion = parse_completion(value).unwrap();
        assert_eq!(completion.message.extra["reasoning_content"], "reason");
        assert_eq!(completion.usage.cached_tokens, 80);
        assert_eq!(completion.usage.total_tokens, 120);
        assert!(completion.usage.reported);
        let tool = Message::tool_result("call1", "result");
        assert_eq!(tool.tool_call_id.as_deref(), Some("call1"));
    }

    #[test]
    fn missing_usage_is_not_silently_treated_as_reported() {
        let completion = parse_completion(json!({"choices":[{
            "message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}))
        .unwrap();
        assert!(!completion.usage.reported);
        assert!(parse_completion(json!({"choices":[]})).is_err());
    }

    #[test]
    fn reasoning_usage_is_reported_only_when_the_provider_counts_it() {
        let base = json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":5}});
        assert_eq!(
            parse_completion(base.clone())
                .unwrap()
                .usage
                .reasoning_tokens,
            None
        );
        let mut with_details = base;
        with_details["usage"]["completion_tokens_details"] = json!({"reasoning_tokens":2});
        assert_eq!(
            parse_completion(with_details)
                .unwrap()
                .usage
                .reasoning_tokens,
            Some(2)
        );
        let usage = Usage {
            reported: true,
            ..Usage::default()
        };
        let mut legacy = serde_json::to_value(&usage).unwrap();
        legacy.as_object_mut().unwrap().remove("reasoning_tokens");
        assert_eq!(
            serde_json::from_value::<Usage>(legacy)
                .unwrap()
                .reasoning_tokens,
            None
        );
    }

    #[test]
    fn streaming_tool_fragments_and_usage_reassemble() {
        let mut stream = StreamAccumulator::default();
        stream.push(json!({"id":"stream1","model":"m","choices":[{"delta":{
            "tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"bash","arguments":"{\"command\":"}}]
        }}]})).unwrap();
        stream
            .push(json!({"choices":[{"delta":{
            "tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]
        },"finish_reason":"tool_calls"}]}))
            .unwrap();
        stream
            .push(
                json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":4,
            "prompt_tokens_details":{"cached_tokens":6}}}),
            )
            .unwrap();
        let completion = parse_completion(stream.finish()).unwrap();
        assert_eq!(
            completion.message.tool_calls[0].function.arguments,
            r#"{"command":"pwd"}"#
        );
        assert_eq!(completion.usage.cached_tokens, 6);
        assert_eq!(completion.finish_reason, "tool_calls");
    }

    #[test]
    fn signed_reasoning_blocks_preserve_stream_order() {
        let blocks = vec![
            json!({"type":"reasoning.text","text":"a","index":0}),
            json!({"type":"reasoning.text","text":"b","signature":"signed","index":0}),
        ];
        let mut stream = StreamAccumulator::default();
        for block in &blocks {
            stream
                .push(json!({"choices":[{"delta":{"reasoning_details":[block]}}]}))
                .unwrap();
        }
        let completion = parse_completion(stream.finish()).unwrap();
        assert_eq!(completion.message.extra["reasoning_details"], json!(blocks));
    }

    #[test]
    fn refusal_deltas_and_content_filter_finish_are_preserved() {
        let mut stream = StreamAccumulator::default();
        for refusal in ["I cannot ", "help with that."] {
            stream
                .push(json!({"choices":[{"delta":{"refusal":refusal}}]}))
                .unwrap();
        }
        stream
            .push(json!({"choices":[{"delta":{},"finish_reason":"content_filter"}]}))
            .unwrap();
        let completion = parse_completion(stream.finish()).unwrap();
        assert_eq!(
            completion.message.extra["refusal"],
            "I cannot help with that."
        );
        assert_eq!(completion.finish_reason, "content_filter");
    }
}
