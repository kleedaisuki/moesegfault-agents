//! Provider continuation contracts:
//! - <https://api-docs.deepseek.com/guides/tool_calls/>
//! - <https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>
//! Reasoning continuation fields belong to persisted messages, not telemetry.

use std::collections::BTreeMap;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

use crate::{
    telemetry::{NativeSpan, TraceContext},
    tools::ToolDefinition,
};

/// Never serialize or Debug this object: API keys must not enter session state or logs.
pub struct ProviderConfig {
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
    let url = worker::Url::parse(&config.base_url)
        .map_err(|_| worker::Error::RustError("invalid provider base URL".into()))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.starts_with('[')
    {
        return Err(worker::Error::RustError(
            "provider requires a public HTTPS hostname".into(),
        ));
    }
    if config.api_key.is_empty() || config.model.is_empty() || config.max_output_tokens == Some(0) {
        return Err(worker::Error::RustError(
            "provider credentials, model, or output limit invalid".into(),
        ));
    }
    Ok(format!(
        "{}/chat/completions",
        config.base_url.trim_end_matches('/')
    ))
}

pub async fn complete(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> worker::Result<Completion> {
    let trace = config.trace_context();
    let _span = NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, messages, tools, false).await?;
    let value: Value = response.json().await?;
    parse_completion(value).map_err(worker::Error::RustError)
}

async fn send(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    stream: bool,
) -> worker::Result<worker::Response> {
    let endpoint = endpoint(config)?;
    let mut body = json!({"model":config.model,"messages":messages,"stream":stream});
    if stream {
        body["stream_options"] = json!({"include_usage":true});
    }
    if worker::Url::parse(&config.base_url)
        .ok()
        .is_some_and(|url| url.host_str() == Some("openrouter.ai"))
    {
        if let Some(session_id) = &config.session_id {
            body["session_id"] = json!(session_id);
        }
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools)?;
    }
    if let Some(limit) = config.max_output_tokens {
        body["max_tokens"] = json!(limit);
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

/// Emits text as soon as the upstream sends it, while preserving full tool and
/// reasoning continuations for the next inference round. Does not buffer SSE.
pub async fn complete_stream<F: FnMut(&str)>(
    config: &ProviderConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    mut on_delta: F,
) -> worker::Result<Completion> {
    let trace = config.trace_context();
    let _span = NativeSpan::start("provider.request", trace.as_ref());
    let mut response = send(config, messages, tools, true).await?;
    let mut events = response.stream()?.eventsource();
    let mut accumulator = StreamAccumulator::default();
    let mut done = false;
    while let Some(event) = events.next().await {
        let event =
            event.map_err(|_| worker::Error::RustError("invalid provider event stream".into()))?;
        if event.data.trim() == "[DONE]" {
            done = true;
            break;
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| worker::Error::RustError("invalid provider stream chunk".into()))?;
        if let Some(text) = accumulator.push(value).map_err(worker::Error::RustError)? {
            on_delta(&text);
        }
    }
    if !done || accumulator.finish_reason.is_none() {
        return Err(worker::Error::RustError(
            "provider stream ended before completion".into(),
        ));
    }
    parse_completion(accumulator.finish()).map_err(worker::Error::RustError)
}

#[derive(Default)]
struct StreamAccumulator {
    id: String,
    model: String,
    content: String,
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
        if self.bytes > 16 * 1024 * 1024 {
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
    fn provider_trace_context_is_explicit_and_validated() {
        let mut config = ProviderConfig {
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
}
