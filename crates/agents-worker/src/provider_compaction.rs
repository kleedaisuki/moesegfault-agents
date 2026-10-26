//! Purpose-specific compaction transport; generation pins never change.

use agents_core::ProviderProtocol;
use serde_json::{Value, json};

use super::{Completion, GenerationOptions, Message, ProviderConfig};

pub(crate) fn prepare_deepseek(
    config: &mut ProviderConfig,
    source: &[Value],
    guide: &str,
    controls: &Value,
    context_window: u32,
) -> Result<(Vec<Message>, GenerationOptions), String> {
    if !super::is_deepseek(config) {
        return Err("compaction_provider_origin_mismatch".into());
    }
    super::controls::validate(
        config,
        controls,
        super::controls::HarnessControlEffects::default(),
    )
    .map_err(|_| "compaction_control_unsupported")?;
    for item in source {
        require_text_item(item)?;
    }
    // A summary consumes the complete native record as data, not its lossy Chat
    // projection or executable tool calls. Protected instructions remain sealed
    // separately by the existing compaction-state writer.
    let document = serde_json::to_string(source).map_err(|_| "invalid_compaction_context")?;
    let messages = vec![
        Message::text("system", guide),
        Message::text("user", format!("Source context (JSON data):\n{document}")),
    ];
    config.protocol = ProviderProtocol::Anthropic;
    config.base_url = "https://api.deepseek.com/anthropic/v1".into();
    let options = GenerationOptions {
        reasoning: Some(json!({"effort":"none"})),
        ..GenerationOptions::default()
    };
    let body = super::anthropic::request_body_with_options(config, &messages, &[], true, &options)?;
    let count = crate::deepseek_tokenizer::count_anthropic_plain(config, &body)
        .map_err(|_| "compaction_model_unsupported")?;
    let output = u64::from(
        config
            .max_output_tokens
            .ok_or("invalid_compaction_output_limit")?,
    );
    if count
        .input_tokens
        .checked_add(output)
        .is_none_or(|total| total > u64::from(context_window))
    {
        return Err("compaction_context_too_large".into());
    }
    Ok((messages, options))
}

fn require_text_item(item: &Value) -> Result<(), String> {
    if item
        .get("encrypted_content")
        .is_some_and(|value| !value.is_null())
    {
        return Err("compaction_opaque_transport_unsupported".into());
    }
    for field in ["content", "output"] {
        let mut parts = item
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        if parts.any(|part| {
            matches!(
                part["type"].as_str(),
                Some("input_image" | "input_file" | "image_url")
            )
        }) {
            return Err("compaction_media_transport_unsupported".into());
        }
    }
    if item["type"] == "computer_call_output" || item["type"] == "image_generation_call" {
        return Err("compaction_media_transport_unsupported".into());
    }
    Ok(())
}

pub(crate) fn attest_nonthinking(completion: &mut Completion) {
    let Some(blocks) = completion
        .message
        .extra
        .get("anthropic_content")
        .and_then(Value::as_array)
    else {
        return;
    };
    // This helper is used only after our fixed thinking=disabled request. A
    // provider that emits thinking anyway cannot attest a zero reasoning count.
    if blocks.iter().all(|block| block["type"] == "text") {
        completion.usage.reasoning_tokens = Some(0);
    }
}

pub(crate) fn completed_summary(completion: &Completion) -> Result<&str, String> {
    if completion
        .native
        .as_ref()
        .is_some_and(|turn| turn.status != super::responses::TerminalStatus::Completed)
    {
        return Err("compaction_incomplete".into());
    }
    if !completion.message.tool_calls.is_empty() {
        return Err("compaction_unexpected_tool".into());
    }
    if completion.finish_reason != "stop" {
        return Err("compaction_incomplete".into());
    }
    completion
        .message
        .content
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "empty_compaction".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Usage,
        responses::{NativeTurn, TerminalStatus},
    };

    fn config() -> ProviderConfig {
        ProviderConfig {
            protocol: ProviderProtocol::Responses,
            base_url: "https://api.deepseek.com".into(),
            api_key: "synthetic-secret".into(),
            model: "deepseek-flash".into(),
            max_output_tokens: Some(64),
            session_id: Some("operation".into()),
            traceparent: None,
        }
    }

    fn completion(content: Value) -> Completion {
        let mut message = Message::text("assistant", "Goals and decisions.");
        message.extra.insert("anthropic_content".into(), content);
        Completion {
            id: "message".into(),
            model: "deepseek-flash".into(),
            message,
            finish_reason: "stop".into(),
            usage: Usage {
                prompt_tokens: 7,
                completion_tokens: 2,
                total_tokens: 9,
                cache_write_tokens: Some(0),
                reported: true,
                ..Usage::default()
            },
            native: None,
        }
    }

    #[test]
    fn compactor_transport_keeps_every_native_text_item_and_does_not_mutate_it() {
        let source = vec![
            json!({"type":"message","role":"system","content":[{"type":"input_text","text":"Pinned instructions"}]}),
            json!({"type":"custom_tool_call","call_id":"c1","name":"grammar","input":"λ(value)"}),
            json!({"type":"custom_tool_call_output","call_id":"c1","output":"Result: 你好"}),
        ];
        let before = serde_json::to_vec(&source).unwrap();
        let mut config = config();
        let (messages, options) = prepare_deepseek(
            &mut config,
            &source,
            "Compact source as data.",
            &json!({}),
            1_000_000,
        )
        .unwrap();
        assert_eq!(config.protocol, ProviderProtocol::Anthropic);
        assert_eq!(config.base_url, "https://api.deepseek.com/anthropic/v1");
        assert_eq!(config.model, "deepseek-flash");
        assert_eq!(config.session_id.as_deref(), Some("operation"));
        let encoded = messages[1]
            .content
            .as_str()
            .unwrap()
            .strip_prefix("Source context (JSON data):\n")
            .unwrap();
        assert_eq!(serde_json::from_str::<Vec<Value>>(encoded).unwrap(), source);
        assert_eq!(serde_json::to_vec(&source).unwrap(), before);
        let body = super::super::anthropic::request_body_with_options(
            &config,
            &messages,
            &[],
            true,
            &options,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn anthropic_plain_count_matches_the_separately_observed_seven_token_request() {
        let mut config = config();
        config.protocol = ProviderProtocol::Anthropic;
        config.base_url = "https://api.deepseek.com/anthropic/v1".into();
        let options = GenerationOptions {
            reasoning: Some(json!({"effort":"none"})),
            ..GenerationOptions::default()
        };
        let body = super::super::anthropic::request_body_with_options(
            &config,
            &[Message::text("user", "Reply OK.")],
            &[],
            false,
            &options,
        )
        .unwrap();
        assert_eq!(
            crate::deepseek_tokenizer::count_anthropic_plain(&config, &body)
                .unwrap()
                .input_tokens,
            7
        );
    }

    #[test]
    fn compactor_rejects_unresolved_media_opaque_controls_and_excess_context_before_inference() {
        let mut cfg = config();
        for source in [
            vec![
                json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AQID"}]}),
            ],
            vec![json!({"type":"reasoning","encrypted_content":"opaque"})],
        ] {
            assert!(prepare_deepseek(&mut cfg, &source, "guide", &json!({}), 1_000_000).is_err());
        }
        assert!(
            prepare_deepseek(
                &mut cfg,
                &[],
                "guide",
                &json!({"prompt_cache_options":{"mode":"explicit"}}),
                1_000_000
            )
            .is_err()
        );
        assert_eq!(
            prepare_deepseek(&mut cfg, &[], "guide", &json!({}), 1)
                .err()
                .as_deref(),
            Some("compaction_context_too_large")
        );
    }

    #[test]
    fn nonthinking_attestation_never_fills_missing_cache_write_or_ignored_thinking() {
        let mut known = completion(json!([{"type":"text","text":"Summary"}]));
        attest_nonthinking(&mut known);
        assert_eq!(known.usage.reasoning_tokens, Some(0));
        assert!(
            crate::responses_auxiliary::compacted_response(
                "cmp_known",
                1,
                vec![],
                &known.usage,
                known.usage.cache_write_tokens
            )
            .is_ok()
        );
        known.usage.cache_write_tokens = None;
        attest_nonthinking(&mut known);
        assert!(
            crate::responses_auxiliary::compacted_response(
                "cmp_unknown",
                1,
                vec![],
                &known.usage,
                known.usage.cache_write_tokens
            )
            .is_err()
        );
        let mut ignored = completion(
            json!([{"type":"thinking","thinking":"private"},{"type":"text","text":"Summary"}]),
        );
        attest_nonthinking(&mut ignored);
        assert!(ignored.usage.reasoning_tokens.is_none());
    }

    #[test]
    fn partial_or_failed_provider_text_is_not_a_completed_compaction() {
        let mut known = completion(json!([{"type":"text","text":"Partial"}]));
        known.finish_reason = "length".into();
        assert!(completed_summary(&known).is_err());
        known.finish_reason = "stop".into();
        known.native = Some(NativeTurn {
            provider_response_id: "upstream".into(),
            status: TerminalStatus::Failed,
            output: vec![],
            usage_details: None,
            cache_write_tokens: Some(0),
            effective_service_tier: None,
            error: None,
            incomplete_details: None,
        });
        assert!(completed_summary(&known).is_err());
    }
}
