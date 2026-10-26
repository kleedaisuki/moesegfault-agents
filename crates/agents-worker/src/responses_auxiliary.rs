//! Standalone Responses operations share rendered context, not generation side effects.

use std::{collections::BTreeMap, time::Duration};

use agents_auth::billing::Resource;
use agents_core::ProviderProtocol;
use futures::{StreamExt, future::Either};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Delay, Env, Error, Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

use crate::provider::{Message, ProviderConfig, Usage};

const MAX_INSTRUCTIONS: usize = 64 * 1024;
const MAX_COMPACTION_STATE: usize = 2 * 1024 * 1024;
const MAX_COUNT_REPLY: usize = 8 * 1024;
const SEALED_PREFIX: &str = "mcmp1.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompactRequest {
    pub model: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub prompt_cache_options: Option<Value>,
    #[serde(default)]
    pub prompt_cache_retention: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CountRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub conversation: Option<Value>,
    // The shared Responses codec validates tool unions and generation policies.
    // Preserve their representation rather than creating a second narrow schema.
    #[serde(flatten)]
    pub options: BTreeMap<String, Value>,
}

fn invalid() -> Error {
    Error::RustError("invalid_auxiliary_request".into())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn owner_context(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024
}

fn common(
    model: Option<&str>,
    instructions: Option<&str>,
    previous: Option<&str>,
) -> worker::Result<()> {
    if model.is_some_and(|value| !identifier(value))
        || previous.is_some_and(|value| !identifier(value))
        || instructions.is_some_and(|value| value.len() > MAX_INSTRUCTIONS || value.contains('\0'))
    {
        return Err(invalid());
    }
    Ok(())
}

impl CompactRequest {
    pub(crate) fn validate(&self) -> worker::Result<()> {
        common(
            self.model.as_deref(),
            self.instructions.as_deref(),
            self.previous_response_id.as_deref(),
        )?;
        if self.model.is_none() && self.previous_response_id.is_none() {
            return Err(invalid());
        }
        if self
            .prompt_cache_key
            .as_ref()
            .is_some_and(|value| value.len() > 256)
            || self
                .prompt_cache_retention
                .as_deref()
                .is_some_and(|value| !matches!(value, "in_memory" | "24h"))
            || self.service_tier.as_deref().is_some_and(|value| {
                !matches!(value, "auto" | "default" | "fast" | "flex" | "priority")
            })
        {
            return Err(invalid());
        }
        if let Some(options) = &self.prompt_cache_options {
            validate_cache(options)?;
        }
        Ok(())
    }
}

impl CountRequest {
    pub(crate) fn validate(&self) -> worker::Result<()> {
        common(
            self.model.as_deref(),
            self.instructions.as_deref(),
            self.previous_response_id.as_deref(),
        )?;
        if self.previous_response_id.is_some() && self.conversation.is_some() {
            return Err(invalid());
        }
        if let Some(conversation) = &self.conversation {
            let id = match conversation {
                Value::String(id) => Some(id.as_str()),
                Value::Object(fields) if fields.len() == 1 => {
                    fields.get("id").and_then(Value::as_str)
                }
                _ => None,
            };
            if !id.is_some_and(identifier) {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

fn validate_cache(value: &Value) -> worker::Result<()> {
    let fields = value.as_object().ok_or_else(invalid)?;
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "mode" | "ttl"))
        || fields
            .get("mode")
            .is_some_and(|value| !matches!(value.as_str(), Some("implicit" | "explicit")))
        || fields.get("ttl").is_some_and(|value| value != "30m")
    {
        return Err(invalid());
    }
    Ok(())
}

/// This is service-owned opaque state, not a fabricated upstream reasoning item.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompactionState {
    version: u8,
    pub model: String,
    pub prefix_hash: String,
    pub instructions: Option<String>,
    pub messages: Vec<Message>,
}

impl CompactionState {
    pub(crate) fn new(
        model: String,
        prefix_hash: String,
        instructions: Option<String>,
        messages: Vec<Message>,
    ) -> worker::Result<Self> {
        let state = Self {
            version: 1,
            model,
            prefix_hash,
            instructions,
            messages,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> worker::Result<()> {
        common(Some(&self.model), self.instructions.as_deref(), None)?;
        if self.version != 1
            || self.prefix_hash.len() != 64
            || !self
                .prefix_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.messages.is_empty()
            || serde_json::to_vec(self)?.len() > MAX_COMPACTION_STATE
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(crate) fn seal(&self, key: &str, owner: &str, id: &str) -> worker::Result<Value> {
        self.validate()?;
        if !owner_context(owner) || !identifier(id) {
            return Err(invalid());
        }
        let context = format!("responses:compaction:{owner}:{id}");
        let encrypted = agents_auth::seal(key, &context, &serde_json::to_vec(self)?)
            .map_err(|_| Error::RustError("compaction_encryption_failed".into()))?;
        Ok(
            json!({"id":id,"type":"compaction","encrypted_content":format!("{SEALED_PREFIX}{encrypted}")}),
        )
    }

    pub(crate) fn open(
        key: &str,
        owner: &str,
        item: &Value,
        model: &str,
        prefix_hash: &str,
    ) -> worker::Result<Self> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| identifier(id))
            .ok_or_else(invalid)?;
        let ciphertext = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .and_then(|value| value.strip_prefix(SEALED_PREFIX))
            .ok_or_else(invalid)?;
        if item.get("type").and_then(Value::as_str) != Some("compaction")
            || !owner_context(owner)
            || ciphertext.len() > MAX_COMPACTION_STATE * 2
        {
            return Err(invalid());
        }
        let context = format!("responses:compaction:{owner}:{id}");
        let bytes = agents_auth::unseal(key, &context, ciphertext).map_err(|_| invalid())?;
        let state: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        state.validate()?;
        if state.model != model || state.prefix_hash != prefix_hash {
            return Err(Error::RustError("compaction_context_mismatch".into()));
        }
        Ok(state)
    }
}

pub(crate) struct ExpandedCompaction {
    pub items: Vec<Value>,
    pub instructions: Option<String>,
}

/// Unseal only this service's compaction items after owner and pin resolution.
/// Protected instructions are returned separately for the common context codec
/// to merge once; they are not compact-only guidance or user-authored messages.
pub(crate) fn expand_compaction_items(
    key: &str,
    owner: &str,
    agent: &str,
    prefix_hash: &str,
    items: &[Value],
) -> worker::Result<ExpandedCompaction> {
    expand_items(key, owner, agent, prefix_hash, items, None)
}

pub(crate) fn expand_for_prefix(
    key: &str,
    owner: &str,
    agent: &str,
    prefix_hash: &str,
    items: &[Value],
    base: &str,
) -> worker::Result<ExpandedCompaction> {
    expand_items(key, owner, agent, prefix_hash, items, Some(base))
}

fn expand_items(
    key: &str,
    owner: &str,
    agent: &str,
    prefix_hash: &str,
    items: &[Value],
    base: Option<&str>,
) -> worker::Result<ExpandedCompaction> {
    let mut output = Vec::with_capacity(items.len());
    let mut instructions = None;
    let mut found = false;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            output.push(item.clone());
            continue;
        }
        if found {
            return Err(invalid());
        }
        found = true;
        let mut state = CompactionState::open(key, owner, item, agent, prefix_hash)?;
        instructions = state.instructions;
        if state.messages.first().is_some_and(|message| {
            message.role == "system"
                && base.is_some_and(|base| message.content.as_str() == Some(base))
        }) {
            state.messages.remove(0);
        }
        output.extend(
            crate::provider::responses::message_items(&state.messages).map_err(|_| invalid())?,
        );
    }
    Ok(ExpandedCompaction {
        items: output,
        instructions,
    })
}

pub(crate) fn compacted_response(
    id: &str,
    created_at: u64,
    output: Vec<Value>,
    usage: &Usage,
    cache_write_tokens: Option<u64>,
) -> worker::Result<Value> {
    let reasoning = usage
        .reasoning_tokens
        .filter(|value| *value <= usage.completion_tokens)
        .ok_or_else(|| Error::RustError("compaction_usage_incomplete".into()))?;
    let cache_write_tokens = cache_write_tokens
        .filter(|value| *value <= usage.prompt_tokens)
        .ok_or_else(|| Error::RustError("compaction_usage_incomplete".into()))?;
    if !identifier(id)
        || !usage.reported
        || usage.cached_tokens > usage.prompt_tokens
        || usage.total_tokens
            != usage
                .prompt_tokens
                .checked_add(usage.completion_tokens)
                .ok_or_else(invalid)?
    {
        return Err(Error::RustError("compaction_usage_incomplete".into()));
    }
    Ok(
        json!({"id":id,"object":"response.compaction","created_at":created_at,"output":output,
        "usage":{"input_tokens":usage.prompt_tokens,"input_tokens_details":{"cached_tokens":usage.cached_tokens,"cache_write_tokens":cache_write_tokens},
            "output_tokens":usage.completion_tokens,"output_tokens_details":{"reasoning_tokens":reasoning},
            "total_tokens":usage.total_tokens}}),
    )
}

#[derive(Clone, Copy)]
pub(crate) enum CountProtocol {
    Responses,
    Anthropic,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputTokenCount {
    pub object: String,
    pub input_tokens: u64,
}

impl InputTokenCount {
    fn parse(protocol: CountProtocol, value: Value) -> worker::Result<Self> {
        if matches!(protocol, CountProtocol::Responses)
            && value.get("object").and_then(Value::as_str) != Some("response.input_tokens")
        {
            return Err(Error::RustError("token_count_invalid_response".into()));
        }
        let input_tokens = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::RustError("token_count_invalid_response".into()))?;
        Ok(Self {
            object: "response.input_tokens".into(),
            input_tokens,
        })
    }
}

/// `body` must use the same resolved context and codec as generation. Platform
/// response/conversation IDs must be resolved before crossing the provider edge.
pub(crate) async fn count_native(
    config: &ProviderConfig,
    protocol: CountProtocol,
    body: Value,
) -> worker::Result<InputTokenCount> {
    if body.get("model").and_then(Value::as_str) != Some(config.model.as_str())
        || body
            .get("previous_response_id")
            .is_some_and(|value| !value.is_null())
        || body
            .get("conversation")
            .is_some_and(|value| !value.is_null())
    {
        return Err(Error::RustError("token_count_context_not_prepared".into()));
    }
    let operation = Box::pin(send_count(config, protocol, body));
    let timeout = Box::pin(Delay::from(Duration::from_secs(30)));
    match futures::future::select(operation, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(Error::RustError("token_count_timeout".into())),
    }
}

/// Count-only work has an account resource reservation, not a model run or a
/// synthetic conversation Session. The caller resolves and authorizes context.
pub(crate) async fn count_account(
    env: &Env,
    owner: &str,
    config: &ProviderConfig,
    protocol: CountProtocol,
    body: Value,
) -> worker::Result<InputTokenCount> {
    let operation = crate::util::id("token_count");
    crate::reconcile::maintenance_account_reservation(env, owner, &operation).await?;
    let meter = crate::resources::ResourceMeter::account(env, owner, &operation)?;
    meter
        .require_observation("provider.count", Resource::Worker, "wall_ms")
        .await?;
    let started = crate::util::now_ms();
    let result = count_native(config, protocol, body).await;
    let observed = meter
        .record_wall_time(
            "provider.count",
            Resource::Worker,
            started,
            crate::util::now_ms(),
        )
        .await;
    if observed.is_ok() {
        meter.settle_run().await?;
    }
    observed?;
    result
}

pub(crate) async fn count_prepared(
    env: &Env,
    owner: &str,
    config: &ProviderConfig,
    prepared: &crate::provider::responses::PreparedRequest,
) -> worker::Result<InputTokenCount> {
    if crate::deepseek_tokenizer::is_deepseek(config) {
        let operation = crate::util::id("token_count");
        crate::reconcile::maintenance_account_reservation(env, owner, &operation).await?;
        let meter = crate::resources::ResourceMeter::account(env, owner, &operation)?;
        meter
            .require_observation("provider.count", Resource::Worker, "wall_ms")
            .await?;
        let started = crate::util::now_ms();
        let counted =
            crate::deepseek_tokenizer::count_deepseek(config, prepared).map_err(Error::RustError);
        meter
            .record_wall_time(
                "provider.count",
                Resource::Worker,
                started,
                crate::util::now_ms(),
            )
            .await?;
        meter.settle_run().await?;
        let count = counted?;
        crate::util::emit(
            "provider.count.local",
            json!({"model":config.model,"input_tokens":count.input_tokens,
            "renderer_revision":count.renderer_revision,"tokenizer_sha256":count.tokenizer_sha256,
            "wasm_linear_memory_bytes":count.wasm_linear_memory_bytes}),
        );
        return Ok(InputTokenCount {
            object: "response.input_tokens".into(),
            input_tokens: count.input_tokens,
        });
    }
    count_account(
        env,
        owner,
        config,
        CountProtocol::Responses,
        prepared.count_body(),
    )
    .await
}

async fn send_count(
    config: &ProviderConfig,
    protocol: CountProtocol,
    body: Value,
) -> worker::Result<InputTokenCount> {
    let trace = config
        .traceparent
        .as_deref()
        .and_then(crate::telemetry::TraceContext::parse);
    let _span = crate::telemetry::NativeSpan::start("provider.count", trace.as_ref());
    let (wire, suffix) = match protocol {
        CountProtocol::Responses => (ProviderProtocol::Openai, "responses/input_tokens"),
        CountProtocol::Anthropic => (ProviderProtocol::Anthropic, "messages/count_tokens"),
    };
    let base = crate::auth::provider_keys::normalize_base_url(wire, &config.base_url)?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    match protocol {
        CountProtocol::Responses => {
            headers.set("Authorization", &format!("Bearer {}", config.api_key))?
        }
        CountProtocol::Anthropic => {
            headers.set("x-api-key", &config.api_key)?;
            headers.set("anthropic-version", "2023-06-01")?;
        }
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_redirect(RequestRedirect::Manual)
        .with_body(Some(body.to_string().into()));
    let mut request =
        Request::new_with_init(&format!("{}/{suffix}", base.trim_end_matches('/')), &init)?;
    if let Some(trace) = trace {
        trace.child().inject(request.headers_mut()?)?;
    }
    let mut response = Fetch::Request(request).send().await?;
    let code = match response.status_code() {
        200 => None,
        404 | 405 | 501 => Some("token_count_unavailable"),
        429 => Some("token_count_overloaded"),
        400 | 422 => Some("token_count_invalid_request"),
        _ => Some("token_count_provider_error"),
    };
    if let Some(code) = code {
        return Err(Error::RustError(code.into()));
    }
    let mut source = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk?;
        if chunk.len() > MAX_COUNT_REPLY.saturating_sub(bytes.len()) {
            return Err(Error::RustError("token_count_invalid_response".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::RustError("token_count_invalid_response".into()))?;
    InputTokenCount::parse(protocol, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auxiliary_controls_keep_request_identity_and_cache_policy() {
        let request: CompactRequest = serde_json::from_value(json!({"model":"agent","input":[],
            "prompt_cache_options":{"mode":"explicit","ttl":"30m"},"service_tier":"fast"}))
        .unwrap();
        request.validate().unwrap();
        for body in [
            json!({"input":[]}),
            json!({"model":"agent","prompt_cache_retention":"forever"}),
            json!({"model":"agent","prompt_cache_options":{"ttl":"24h"}}),
        ] {
            assert!(
                serde_json::from_value::<CompactRequest>(body)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let request: CountRequest = serde_json::from_value(
            json!({"model":"agent","conversation":"conv_1","previous_response_id":"resp_1"}),
        )
        .unwrap();
        assert!(request.validate().is_err());
    }

    #[test]
    fn compaction_state_cannot_cross_owner_item_model_or_prefix() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let hash = "a".repeat(64);
        let state = CompactionState::new(
            "agent".into(),
            hash.clone(),
            Some("exact custom 雪".into()),
            vec![
                Message::text("system", "Exact pinned prefix"),
                Message::text("user", "summary"),
            ],
        )
        .unwrap();
        let item = state.seal(key, "owner", "cmp_1").unwrap();
        let decoded = CompactionState::open(key, "owner", &item, "agent", &hash).unwrap();
        assert_eq!(decoded.instructions, state.instructions);
        assert_eq!(
            serde_json::to_value(decoded.messages).unwrap(),
            serde_json::to_value(state.messages).unwrap()
        );
        assert!(CompactionState::open(key, "other", &item, "agent", &hash).is_err());
        assert!(CompactionState::open(key, "owner", &item, "other", &hash).is_err());
        assert!(CompactionState::open(key, "owner", &item, "agent", &"b".repeat(64)).is_err());
        let mut changed = item;
        changed["id"] = json!("cmp_other");
        assert!(CompactionState::open(key, "owner", &changed, "agent", &hash).is_err());
    }

    #[test]
    fn standard_compaction_usage_never_invents_reasoning_counts() {
        let mut usage = Usage {
            prompt_tokens: 12,
            completion_tokens: 3,
            total_tokens: 15,
            cached_tokens: 4,
            reasoning_tokens: Some(1),
            reported: true,
            ..Usage::default()
        };
        let response = compacted_response("cmp_response", 123, vec![], &usage, Some(0)).unwrap();
        assert_eq!(response["object"], "response.compaction");
        assert_eq!(
            response["usage"]["output_tokens_details"]["reasoning_tokens"],
            1
        );
        usage.reasoning_tokens = None;
        assert!(compacted_response("cmp_response", 123, vec![], &usage, Some(0)).is_err());
    }

    #[test]
    fn expanding_compaction_preserves_user_items_and_protected_instructions() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let prefix = "c".repeat(64);
        let state = CompactionState::new(
            "agent".into(),
            prefix.clone(),
            Some("Keep exact instructions".into()),
            vec![
                Message::text("developer", "Protected policy"),
                Message::text("assistant", "Continuation summary"),
            ],
        )
        .unwrap();
        let item = state.seal(key, "owner", "cmp_item").unwrap();
        let user = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Original request"}]});
        let expanded = expand_compaction_items(
            key,
            "owner",
            "agent",
            &prefix,
            &[user.clone(), item.clone()],
        )
        .unwrap();
        assert_eq!(expanded.items[0], user);
        assert_eq!(expanded.items[1]["role"], "developer");
        assert_eq!(expanded.items[2]["role"], "assistant");
        assert_eq!(
            expanded.instructions.as_deref(),
            Some("Keep exact instructions")
        );
        assert!(
            expand_compaction_items(key, "owner", "agent", &prefix, &[item.clone(), item]).is_err()
        );
    }

    #[test]
    fn count_replies_accept_only_provider_reported_unsigned_counts() {
        assert_eq!(
            InputTokenCount::parse(
                CountProtocol::Responses,
                json!({"object":"response.input_tokens","input_tokens":42})
            )
            .unwrap()
            .input_tokens,
            42
        );
        assert_eq!(
            InputTokenCount::parse(CountProtocol::Anthropic, json!({"input_tokens":0}))
                .unwrap()
                .input_tokens,
            0
        );
        for value in [
            json!({"object":"response.input_tokens","input_tokens":-1}),
            json!({"object":"response.input_tokens","input_tokens":1.5}),
            json!({"input_tokens":3}),
        ] {
            assert!(InputTokenCount::parse(CountProtocol::Responses, value).is_err());
        }
    }
}
