//! Native transport support is not evidence that a provider honors a control.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ProviderConfig;

/// Only trusted harness preparation can attest these effects. Caller JSON never
/// deserializes this type directly; persisted prepared requests retain it.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct HarnessControlEffects {
    pub max_tool_calls: bool,
    pub truncation: bool,
    pub single_tool_call: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    DeepSeek,
    OpenRouter,
    Other,
}

fn profile(config: &ProviderConfig) -> Profile {
    match worker::Url::parse(&config.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .as_deref()
    {
        Some("api.deepseek.com") => Profile::DeepSeek,
        Some("openrouter.ai") => Profile::OpenRouter,
        _ => Profile::Other,
    }
}

fn requested(body: &Value, field: &str) -> bool {
    body.get(field).is_some_and(|value| !value.is_null())
}

fn unavailable(field: &str) -> String {
    format!("native provider does not honor {field}; select a capable model/provider")
}

pub(crate) fn validate(
    config: &ProviderConfig,
    body: &Value,
    effects: HarnessControlEffects,
) -> Result<(), String> {
    for (field, max) in [("temperature", 2.0), ("top_p", 1.0)] {
        if requested(body, field)
            && !body[field]
                .as_f64()
                .is_some_and(|value| value.is_finite() && (0.0..=max).contains(&value))
        {
            return Err(format!("invalid native {field}"));
        }
    }
    if requested(body, "text") {
        if let Some(format) = body["text"].get("format") {
            super::validate_text_format(format)?;
        }
    }
    let provider = profile(config);
    if provider == Profile::OpenRouter {
        // The live public ResponsesRequest schema does not declare these OpenAI
        // vendor extensions. Do not equate OpenRouter transport with OpenAI.
        for field in [
            "moderation",
            "access_programs",
            "prompt_cache_retention",
            "personality",
        ] {
            if requested(body, field) {
                return Err(unavailable(field));
            }
        }
    }
    if provider != Profile::DeepSeek {
        return Ok(());
    }
    for field in [
        "prompt_cache_key",
        "prompt_cache_options",
        "prompt_cache_retention",
        "safety_identifier",
        "moderation",
        "access_programs",
        "personality",
    ] {
        if requested(body, field) {
            return Err(unavailable(field));
        }
    }
    if requested(body, "service_tier")
        && !matches!(body["service_tier"].as_str(), Some("auto" | "default"))
    {
        return Err(unavailable("service_tier"));
    }
    if requested(body, "max_tool_calls") && !effects.max_tool_calls {
        return Err(unavailable("max_tool_calls (harness counter required)"));
    }
    if body["parallel_tool_calls"] == false && !effects.single_tool_call {
        return Err(unavailable("parallel_tool_calls=false"));
    }
    if body["truncation"] == "auto" && !effects.truncation {
        return Err(unavailable(
            "truncation=auto (harness context preparation required)",
        ));
    }
    if requested(&body["reasoning"], "summary") || requested(&body["reasoning"], "generate_summary")
    {
        return Err(unavailable("reasoning.summary"));
    }
    let nonthinking = body["reasoning"]["effort"] == "none";
    if requested(body, "temperature") && !nonthinking {
        return Err(unavailable("temperature in thinking mode"));
    }
    if let Some(top_p) = body["top_p"].as_f64() {
        let supported = if config.model.contains("flash") {
            !nonthinking && top_p >= 0.95
        } else if config.model.contains("pro") {
            nonthinking
        } else {
            false
        };
        if !supported {
            return Err(unavailable("top_p in the selected reasoning mode"));
        }
    }
    if let Some(user) = body["user"].as_str() {
        if user.is_empty()
            || user.len() > 512
            || !user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "DeepSeek user must contain 1..512 ASCII letters, digits, underscores or hyphens"
                    .into(),
            );
        }
    }
    Ok(())
}

pub(crate) fn prepare(
    config: &ProviderConfig,
    body: &mut Value,
    effects: HarnessControlEffects,
) -> Result<(), String> {
    validate(config, body, effects)?;
    if profile(config) != Profile::DeepSeek {
        return Ok(());
    }
    if let Some(verbosity) = body["text"].get("verbosity").and_then(Value::as_str) {
        let instruction = match verbosity {
            "low" => {
                "Answer concisely. Prefer only the essential information needed to satisfy the request."
            }
            "medium" => {
                "Use a balanced level of detail, sufficient to explain the answer without unnecessary elaboration."
            }
            "high" => {
                "Provide a detailed and comprehensive answer, explaining the relevant reasoning and useful examples."
            }
            _ => return Err("invalid native text.verbosity".into()),
        };
        // Response-scoped adaptation goes after the protected prefix. Counting
        // and generation share this exact rendered item, never an uncounted hint.
        body["input"].as_array_mut().ok_or("native input must be resolved")?
            .push(json!({"type":"message","role":"system","content":[{"type":"input_text","text":instruction}]}));
        body["text"].as_object_mut().unwrap().remove("verbosity");
    }
    // These effects are either truthful defaults or have an explicit harness
    // attestation. Never send fields that DeepSeek silently ignores.
    for field in [
        "include",
        "service_tier",
        "parallel_tool_calls",
        "max_tool_calls",
        "truncation",
    ] {
        body.as_object_mut().unwrap().remove(field);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(host: &str) -> ProviderConfig {
        ProviderConfig {
            protocol: agents_core::ProviderProtocol::Responses,
            base_url: format!("https://{host}/v1"),
            api_key: "private".into(),
            model: "deepseek-flash".into(),
            max_output_tokens: Some(128),
            session_id: None,
            traceparent: None,
        }
    }
    #[test]
    fn deepseek_ignored_controls_fail_before_financial_admission() {
        let config = config("api.deepseek.com");
        for control in [
            json!({"prompt_cache_key":"key"}),
            json!({"service_tier":"priority"}),
            json!({"safety_identifier":"user"}),
            json!({"moderation":{}}),
            json!({"access_programs":{}}),
            json!({"reasoning":{"summary":"auto"}}),
            json!({"parallel_tool_calls":false}),
            json!({"max_tool_calls":2}),
            json!({"truncation":"auto"}),
            json!({"temperature":0.2}),
        ] {
            assert!(
                validate(&config, &control, HarnessControlEffects::default()).is_err(),
                "{control}"
            );
        }
        validate(&config,&json!({"user":"caller_1","top_logprobs":3,"reasoning":{"effort":"none"},"temperature":0.2}),HarnessControlEffects::default()).unwrap();
    }
    #[test]
    fn verbosity_is_real_counted_context_without_prefix_mutation() {
        let mut body = json!({"input":[{"role":"system","content":"pinned"}],"text":{"verbosity":"high","format":{"type":"text"}},"service_tier":"default"});
        prepare(
            &config("api.deepseek.com"),
            &mut body,
            HarnessControlEffects::default(),
        )
        .unwrap();
        assert_eq!(body["input"][0]["content"], "pinned");
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert!(body["text"].get("verbosity").is_none());
        assert!(body.get("service_tier").is_none());
    }
    #[test]
    fn compatible_byok_retains_vendor_features_and_harness_effects_are_explicit() {
        let body = json!({"prompt_cache_key":"k","moderation":{"model":"moderator"},"reasoning":{"summary":"auto"}});
        validate(
            &config("api.openai.com"),
            &body,
            HarnessControlEffects::default(),
        )
        .unwrap();
        validate(
            &config("custom.example"),
            &body,
            HarnessControlEffects::default(),
        )
        .unwrap();
        assert!(
            validate(
                &config("openrouter.ai"),
                &body,
                HarnessControlEffects::default()
            )
            .is_err()
        );
        let mut body =
            json!({"input":[],"max_tool_calls":1,"parallel_tool_calls":false,"truncation":"auto"});
        prepare(
            &config("api.deepseek.com"),
            &mut body,
            HarnessControlEffects {
                max_tool_calls: true,
                truncation: true,
                single_tool_call: true,
            },
        )
        .unwrap();
        assert!(body.get("max_tool_calls").is_none());
    }
}
