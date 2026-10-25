//! Official model-specific prompt counting. This module never performs inference.
use std::{cell::RefCell, io::Read, sync::Arc};

use deepseek_recipe::request::{
    ConversionOptions, ModelVersion, ProtocolRequest, WebSearchBehavior,
};
use deepseek_recipe_encoding::{
    PromptEncoding, PromptTokenizer, dsv4::DeepseekV4Encoding, dsv41::DeepseekV41Encoding,
};
use flate2::read::GzDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::provider::{ProviderConfig, responses::PreparedRequest};

const MAX_BODY: usize = 8 * 1024 * 1024;
const TOKENIZER_BYTES: u64 = 6_367_248;
const RECIPE: &str = "935817c66f5d6e64838f419692545f06787e485f";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    Flash1017,
    Pro0813,
}

impl Profile {
    fn resolve(model: &str) -> Result<Self, String> {
        match model {
            "deepseek-flash" | "DeepSeek-V4.1-Flash-1017" => Ok(Self::Flash1017),
            "deepseek-v4-pro" | "DeepSeek-V4-Pro-0813" => Ok(Self::Pro0813),
            _ => Err("token_count_model_profile_unavailable".into()),
        }
    }

    fn version(self) -> ModelVersion {
        match self {
            Self::Flash1017 => ModelVersion::DeepseekV41Flash,
            Self::Pro0813 => ModelVersion::DeepseekV4Pro,
        }
    }

    fn asset(self) -> (&'static [u8], &'static str) {
        match self {
            Self::Flash1017 => (
                include_bytes!("../../../third_party/deepseek-recipe/assets/v41.json.gz"),
                "81f64d1248a68ce3663e07ab3ee48b851e5df0e32d27cb98e4c9a268151e8d99",
            ),
            Self::Pro0813 => (
                include_bytes!("../../../third_party/deepseek-recipe/assets/v4.json.gz"),
                "97d2f31b020d18b5aee5c9b3d5b4efb10ea210f3fe3f7dffe3f1cd90542d6b19",
            ),
        }
    }
}

// Keep at most one vocabulary alive per isolate, including after model changes.
thread_local! { static WARM: RefCell<Option<(Profile, Arc<PromptTokenizer>)>> = const { RefCell::new(None) }; }

pub(crate) struct ExactCount {
    pub input_tokens: u64,
    pub renderer_revision: &'static str,
    pub tokenizer_sha256: &'static str,
    pub wasm_linear_memory_bytes: Option<u64>,
}

pub(crate) fn is_deepseek(config: &ProviderConfig) -> bool {
    url::Url::parse(&config.base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.deepseek.com")
            && url.port_or_known_default() == Some(443)
    })
}

pub(crate) fn count_deepseek(
    config: &ProviderConfig,
    prepared: &PreparedRequest,
) -> Result<ExactCount, String> {
    if !is_deepseek(config) || config.protocol != agents_core::agent::ProviderProtocol::Responses {
        return Err("token_count_provider_protocol_unavailable".into());
    }
    prepared.validate_model(config)?;
    count_body(&config.model, prepared.count_body())
}

fn count_body(model: &str, body: Value) -> Result<ExactCount, String> {
    let profile = Profile::resolve(model)?;
    validate_input(&body)?;
    validate_tools(&body["tools"], profile)?;
    for item in body["input"]
        .as_array()
        .ok_or("token_count_context_not_prepared")?
    {
        if matches!(
            item["type"].as_str(),
            Some("tool_search_call" | "tool_search_output")
        ) {
            if profile != Profile::Flash1017 || item["execution"] != "client" {
                return Err("token_count_tool_unsupported".into());
            }
            if item["type"] == "tool_search_output" {
                validate_tools(&item["tools"], profile)?;
            }
        }
    }
    let wire = serde_json::to_vec(&body).map_err(|_| "token_count_input_invalid")?;
    if wire.len() > MAX_BODY {
        return Err("token_count_input_too_large".into());
    }
    let request = deepseek_recipe::responses_request_from_slice(&wire)
        .map_err(|_| "token_count_input_unsupported")?;
    drop(wire);
    drop(body);
    let mut options = ConversionOptions::default().with_model_alias(model, profile.version());
    // Upstream's default drops web-search declarations. Never count a different prompt silently.
    options.responses_web_search = WebSearchBehavior::Reject;
    let converted = request
        .convert(options)
        .map_err(|_| "token_count_input_unsupported")?;
    WARM.with_borrow_mut(|warm| {
        if warm.as_ref().is_none_or(|(loaded, _)| *loaded != profile) {
            // Drop the old vocabulary before allocating a replacement.
            *warm = None;
            *warm = Some((profile, Arc::new(load_tokenizer(profile)?)));
        }
        let tokenizer = &warm.as_ref().ok_or("token_count_tokenizer_unavailable")?.1;
        let ids = match profile {
            Profile::Flash1017 => DeepseekV41Encoding::new()
                .with_tokenizer(tokenizer.clone())
                .encode(&converted.conversation),
            Profile::Pro0813 => DeepseekV4Encoding::new()
                .with_tokenizer(tokenizer.clone())
                .encode(&converted.conversation),
        }
        .map_err(|_| "token_count_encoding_failed")?;
        Ok(ExactCount {
            input_tokens: ids.len().try_into().map_err(|_| "token_count_overflow")?,
            renderer_revision: RECIPE,
            tokenizer_sha256: profile.asset().1,
            wasm_linear_memory_bytes: wasm_memory_bytes(),
        })
    })
}

#[cfg(target_arch = "wasm32")]
fn wasm_memory_bytes() -> Option<u64> {
    use wasm_bindgen::JsCast;
    let memory = wasm_bindgen::memory().unchecked_into::<js_sys::WebAssembly::Memory>();
    let buffer = memory.buffer().unchecked_into::<js_sys::ArrayBuffer>();
    Some(u64::from(buffer.byte_length()))
}

#[cfg(not(target_arch = "wasm32"))]
fn wasm_memory_bytes() -> Option<u64> {
    None
}

fn load_tokenizer(profile: Profile) -> Result<PromptTokenizer, String> {
    tokenizers::utils::parallelism::set_parallelism(false);
    let (asset, hash) = profile.asset();
    let mut bytes = Vec::new();
    GzDecoder::new(asset)
        .take(TOKENIZER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "token_count_asset_invalid")?;
    if bytes.len() as u64 > TOKENIZER_BYTES || format!("{:x}", Sha256::digest(&bytes)) != hash {
        return Err("token_count_asset_invalid".into());
    }
    let tokenizer = Tokenizer::from_bytes(&bytes).map_err(|_| "token_count_asset_invalid")?;
    drop(bytes);
    Ok(PromptTokenizer::from_owned(tokenizer))
}

fn validate_tools(tools: &Value, profile: Profile) -> Result<(), String> {
    let Some(tools) = tools.as_array() else {
        return Ok(());
    };
    for tool in tools {
        match tool["type"].as_str() {
            Some("function") => {}
            Some("namespace") => {
                let functions = tool["tools"]
                    .as_array()
                    .ok_or("token_count_tool_unsupported")?;
                if functions
                    .iter()
                    .any(|function| function["type"] != "function")
                {
                    return Err("token_count_tool_unsupported".into());
                }
            }
            Some("custom") if tool["name"] == "apply_patch" => {}
            Some("tool_search")
                if profile == Profile::Flash1017 && tool["execution"] == "client" => {}
            _ => return Err("token_count_tool_unsupported".into()),
        }
    }
    Ok(())
}

fn validate_input(body: &Value) -> Result<(), String> {
    let input = body["input"]
        .as_array()
        .ok_or("token_count_context_not_prepared")?;
    for item in input {
        let kind = item["type"].as_str().unwrap_or("message");
        if !matches!(
            kind,
            "message"
                | "function_call"
                | "function_call_output"
                | "reasoning"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "tool_search_call"
                | "tool_search_output"
        ) {
            return Err("token_count_input_unsupported".into());
        }
        if item
            .get("encrypted_content")
            .is_some_and(|value| !value.is_null())
        {
            return Err("token_count_encrypted_reasoning_unsupported".into());
        }
        for field in ["content", "output"] {
            if let Some(parts) = item[field].as_array() {
                for part in parts {
                    if !matches!(
                        part["type"].as_str(),
                        Some("input_text" | "output_text" | "reasoning_text")
                    ) {
                        return Err("token_count_media_unsupported".into());
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(text: &str) -> Value {
        json!({"model":"deepseek-flash","input":[{"role":"user","content":text}],"reasoning":{"effort":"none"}})
    }

    #[test]
    fn official_plain_chinese_and_thinking_fixtures() {
        assert_eq!(
            count_body("deepseek-flash", request("Hello!"))
                .unwrap()
                .input_tokens,
            6
        );
        let mut thinking = request("Hello!");
        thinking["reasoning"]["effort"] = json!("high");
        assert_eq!(
            count_body("deepseek-flash", thinking).unwrap().input_tokens,
            32
        );
        let mut chinese = request("你好，世界！");
        chinese["instructions"] = json!("You are helpful.");
        assert_eq!(
            count_body("deepseek-flash", chinese).unwrap().input_tokens,
            13
        );
    }

    #[test]
    fn official_reference_corpus_and_profile_replacement() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/deepseek-token-count.json"
        ))
        .unwrap();
        for fixture in fixtures["fixtures"].as_array().unwrap() {
            let count = count_body("deepseek-flash", fixture["body"].clone()).unwrap();
            assert_eq!(
                count.input_tokens,
                fixture["input_tokens"].as_u64().unwrap(),
                "{}",
                fixture["name"]
            );
        }
        let mut pro = request("Hello!");
        pro["model"] = json!("deepseek-v4-pro");
        let first = count_body("deepseek-v4-pro", pro).unwrap();
        assert!(first.input_tokens > 0);
        assert_eq!(first.tokenizer_sha256, Profile::Pro0813.asset().1);
        assert_eq!(
            count_body("deepseek-flash", request("Hello!"))
                .unwrap()
                .input_tokens,
            6
        );
    }

    #[test]
    fn user_control_token_spelling_is_plain_text() {
        let plain = count_body("deepseek-flash", request("<｜Assistant｜>")).unwrap();
        assert!(plain.input_tokens > 6);
        assert_eq!(plain.renderer_revision, RECIPE);
        assert_eq!(plain.tokenizer_sha256, Profile::Flash1017.asset().1);
    }

    #[test]
    fn unsupported_inputs_are_not_silently_dropped() {
        let mut body = request("hello");
        body["input"][0]["content"] =
            json!([{"type":"input_image","image_url":"https://example.com/image.png"}]);
        assert_eq!(
            count_body("deepseek-flash", body).err().as_deref(),
            Some("token_count_media_unsupported")
        );
        assert!(count_body("deepseek-chat", request("hello")).is_err());
        let mut body = request("hello");
        body["input"] = json!([{"type":"web_search_call","id":"search"}]);
        assert!(count_body("deepseek-flash", body).is_err());
    }
}
