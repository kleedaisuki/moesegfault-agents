//! Responses wire objects are independent of durable execution and provider routing.
//!
//! The public output is deliberately not a serialization of a provider message:
//! private reasoning continuations and provider-specific fields stay in the journal.
//! Contract: https://developers.openai.com/api/reference/resources/responses

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    provider::{FunctionCall, GenerationOptions, Message, ToolCall, Usage},
    tools::ToolDefinition,
};

const MAX_INPUT_ITEMS: usize = 128;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGES: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Vec<ResponseTool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub background: bool,
    #[serde(default = "yes")]
    pub store: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub text: Option<Value>,
    #[serde(default)]
    pub reasoning: Option<Value>,
    #[serde(default)]
    pub truncation: Option<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub stream_options: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "empty_schema")]
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
}

#[derive(Clone, Debug, Default)]
pub struct NormalizedInput {
    pub messages: Vec<Message>,
    pub instructions: Vec<String>,
    pub history_instructions: Vec<Message>,
    pub input_items: Vec<Value>,
    pub tool_outputs: Vec<ToolOutput>,
}

fn yes() -> bool {
    true
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn object_keys(value: &Value, allowed: &[&str], label: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{label} contains an unsupported field"));
    }
    Ok(())
}

fn identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(format!("{label} must be an opaque identifier"));
    }
    Ok(())
}

fn bounded_text(value: &str, limit: usize, label: &str) -> Result<(), String> {
    if value.len() > limit || value.contains('\0') {
        return Err(format!("{label} exceeds its text limit or contains NUL"));
    }
    Ok(())
}

impl ResponseRequest {
    pub fn validate(&self) -> Result<(), String> {
        identifier(&self.model, "model")?;
        if let Some(id) = &self.previous_response_id {
            identifier(id, "previous_response_id")?;
        }
        if !self.extra.is_empty() {
            return Err("Request contains an unsupported Responses parameter".into());
        }
        if let Some(instructions) = &self.instructions {
            bounded_text(instructions, MAX_INSTRUCTIONS_BYTES, "instructions")?;
        }
        if self.max_output_tokens == Some(0) {
            return Err("max_output_tokens must be positive".into());
        }
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
        {
            return Err("temperature must be between 0 and 2".into());
        }
        if self
            .top_p
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err("top_p must be between 0 and 1".into());
        }
        if self.metadata.len() > 16
            || self
                .metadata
                .iter()
                .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 512)
        {
            return Err(
                "metadata supports at most 16 keys of 64 characters and values of 512 characters"
                    .into(),
            );
        }
        if self
            .truncation
            .as_deref()
            .is_some_and(|value| !matches!(value, "auto" | "disabled"))
        {
            return Err("truncation must be auto or disabled".into());
        }
        if !self.include.is_empty() {
            return Err(
                "The requested include fields are not supported by this agent endpoint".into(),
            );
        }
        if self.background && !self.store {
            return Err("background requires store=true so the result can be recovered".into());
        }
        if let Some(options) = &self.stream_options {
            object_keys(options, &["include_obfuscation"], "stream_options")?;
            if !self.stream {
                return Err("stream_options requires stream=true".into());
            }
            if options
                .get("include_obfuscation")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err("include_obfuscation must be a boolean".into());
            }
        }
        self.validate_tools()?;
        self.validate_text()?;
        Ok(())
    }

    fn validate_tools(&self) -> Result<(), String> {
        if self.tools.len() > 128 {
            return Err("At most 128 client tools may be registered".into());
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if tool.kind != "function" {
                return Err("Only client function tools are supported in tools; agent built-ins are configured on the server".into());
            }
            if tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err("Function names must contain 1 to 64 ASCII letters, digits, underscores or hyphens".into());
            }
            if !names.insert(&tool.name) {
                return Err("Client function names must be unique".into());
            }
            if let Some(description) = &tool.description {
                bounded_text(description, MAX_INSTRUCTIONS_BYTES, "function description")?;
            }
            if !tool.parameters.is_object()
                || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
                || tool
                    .parameters
                    .get("properties")
                    .is_some_and(|value| !value.is_object())
            {
                return Err("Function parameters must be an object JSON Schema".into());
            }
            if serde_json::to_vec(&tool.parameters)
                .map_err(|_| "Invalid function schema")?
                .len()
                > MAX_INSTRUCTIONS_BYTES
            {
                return Err("Function schema exceeds 64 KiB".into());
            }
            crate::responses_schema::validate_schema(&tool.parameters)?;
        }
        Ok(())
    }

    fn validate_text(&self) -> Result<(), String> {
        let Some(text) = &self.text else {
            return Ok(());
        };
        object_keys(text, &["format", "verbosity"], "text")?;
        if text.get("verbosity").is_some() {
            return Err("text.verbosity is not supported by the configured providers".into());
        }
        let Some(format) = text.get("format") else {
            return Ok(());
        };
        let kind = format
            .get("type")
            .and_then(Value::as_str)
            .ok_or("text.format.type is required")?;
        match kind {
            "text" | "json_object" => object_keys(format, &["type"], "text.format"),
            "json_schema" => {
                object_keys(
                    format,
                    &["type", "name", "description", "schema", "strict"],
                    "text.format",
                )?;
                let name = format
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("JSON schema format requires a name")?;
                identifier(name, "JSON schema format name")?;
                if name.len() > 64 || !format.get("schema").is_some_and(Value::is_object) {
                    return Err("JSON schema format requires a name of at most 64 bytes and an object schema".into());
                }
                if format
                    .get("strict")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err("JSON schema strict must be a boolean".into());
                }
                if format
                    .get("description")
                    .is_some_and(|value| !value.is_string())
                {
                    return Err("JSON schema description must be a string".into());
                }
                crate::responses_schema::validate_schema(&format["schema"])?;
                Ok(())
            }
            _ => Err("Unsupported text.format.type".into()),
        }
    }

    pub fn generation_options(&self) -> GenerationOptions {
        GenerationOptions {
            temperature: self.temperature,
            top_p: self.top_p,
            parallel_tool_calls: self.parallel_tool_calls,
            tool_choice: self.tool_choice.clone(),
            text_format: self
                .text
                .as_ref()
                .and_then(|text| text.get("format"))
                .cloned(),
            reasoning: self.reasoning.clone(),
        }
    }

    pub fn tool_definitions(&self) -> Result<Vec<ToolDefinition>, String> {
        self.validate_tools()?;
        self.tools
            .iter()
            .map(|tool| {
                serde_json::from_value(json!({
                    "type":"function",
                    "function":{
                        "name":tool.name,
                        "description":tool.description.as_deref().unwrap_or(""),
                        "parameters":tool.parameters,
                        "strict":tool.strict,
                    }
                }))
                .map_err(|_| "Invalid client function definition".into())
            })
            .collect()
    }

    pub fn normalize(&self) -> Result<NormalizedInput, String> {
        self.validate()?;
        self.normalize_input(&self.input)
    }

    /// Syntax-only preflight before allocating a session. File ownership and
    /// content resolution still happen through Space before actual execution.
    pub fn validate_input_structure(&self) -> Result<(), String> {
        self.normalize_structure().map(|_| ())
    }

    pub fn normalize_structure(&self) -> Result<NormalizedInput, String> {
        self.validate()?;
        let mut input = self.input.clone();
        let mut images = 0;
        placeholder_files(&mut input, &mut images)?;
        let mut normalized = self.normalize_input(&input)?;
        // Public input history keeps owner-scoped references, not a second copy
        // of bytes already durably stored in Space.
        normalized.input_items = input_items(&self.input)?;
        Ok(normalized)
    }

    fn normalize_input(&self, input: &Value) -> Result<NormalizedInput, String> {
        let mut normalized = NormalizedInput::default();
        if let Some(instructions) = &self.instructions {
            normalized.instructions.push(instructions.clone());
        }
        let items = raw_input_items(input)?;
        for item in items {
            normalize_item(&item, &mut normalized)?;
            normalized.input_items.push(canonical_input_item(&item)?);
        }
        let instruction_bytes = normalized
            .instructions
            .iter()
            .map(String::len)
            .sum::<usize>()
            + normalized
                .history_instructions
                .iter()
                .filter_map(|message| message.content.as_str())
                .map(str::len)
                .sum::<usize>();
        if instruction_bytes > MAX_INSTRUCTIONS_BYTES {
            return Err("Combined custom instructions exceed 64 KiB".into());
        }
        let mut outputs = BTreeSet::new();
        if normalized
            .tool_outputs
            .iter()
            .any(|output| !outputs.insert(&output.call_id))
        {
            return Err("Each function call may have only one output in a request".into());
        }
        validate_image_budget(&normalized.messages)?;
        Ok(normalized)
    }
}

fn raw_input_items(input: &Value) -> Result<Vec<Value>, String> {
    match input {
        Value::Null => Ok(vec![]),
        Value::String(text) => Ok(vec![json!({"type":"message","role":"user","content":text})]),
        Value::Array(items) if items.len() <= MAX_INPUT_ITEMS => Ok(items.clone()),
        _ => Err("input must be text or an array of at most 128 input items".into()),
    }
}

fn input_items(input: &Value) -> Result<Vec<Value>, String> {
    raw_input_items(input)?
        .iter()
        .map(canonical_input_item)
        .collect()
}

fn placeholder_files(value: &mut Value, images: &mut usize) -> Result<(), String> {
    match value {
        Value::Object(object)
            if object.get("type").and_then(Value::as_str) == Some("input_file") =>
        {
            let part = Value::Object(object.clone());
            object_keys(&part, &["type", "file_id"], "input_file")?;
            identifier(required_string(&part, "file_id")?, "file_id")?;
            *value = json!({"type":"input_text","text":""});
        }
        Value::Object(object)
            if object.get("type").and_then(Value::as_str) == Some("input_image") =>
        {
            *images += 1;
            if *images > MAX_IMAGES {
                return Err("A response accepts at most 8 images totaling 4 MiB".into());
            }
            if object.contains_key("file_id") {
                let part = Value::Object(object.clone());
                object_keys(&part, &["type", "file_id", "detail"], "input_image")?;
                identifier(required_string(&part, "file_id")?, "file_id")?;
                image_detail(part.get("detail"))?;
                *value = json!({"type":"input_text","text":""});
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                placeholder_files(value, images)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                placeholder_files(value, images)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn canonical_input_item(item: &Value) -> Result<Value, String> {
    if !item.is_object() {
        return Err("Input items must be objects".into());
    }
    let mut item = item.clone();
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_owned();
    item["type"] = json!(kind);
    item["status"] = json!("completed");
    if kind == "message"
        && let Some(text) = item["content"].as_str()
    {
        item["content"] = if item["role"] == "assistant" {
            json!([{"type":"output_text","text":text,"annotations":[],"logprobs":[]}])
        } else {
            json!([{"type":"input_text","text":text}])
        };
    }
    if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("input_image")
                && part.get("detail").is_none()
            {
                part["detail"] = json!("auto");
            }
            if part.get("type").and_then(Value::as_str) == Some("output_text")
                && part.get("annotations").is_none()
            {
                part["annotations"] = json!([]);
            }
        }
    }
    Ok(item)
}

/// Bind easy input items to their immutable response before publishing them from
/// the input-items endpoint. A caller-supplied item ID is not an ownership grant.
pub fn identified_input_items(items: &[Value], id_prefix: &str) -> Vec<Value> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut item = item.clone();
            if item.get("id").and_then(Value::as_str).is_none() {
                item["id"] = json!(format!("{id_prefix}_input_{index}"));
            }
            item
        })
        .collect()
}

pub fn canonical_input_items(input: &Value, id_prefix: &str) -> Result<Vec<Value>, String> {
    Ok(identified_input_items(&input_items(input)?, id_prefix))
}

fn required_string<'a>(item: &'a Value, key: &str) -> Result<&'a str, String> {
    item.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Input item requires string {key}"))
}

fn normalize_item(item: &Value, normalized: &mut NormalizedInput) -> Result<(), String> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => normalize_message(item, normalized),
        "function_call" => {
            object_keys(
                item,
                &["type", "id", "call_id", "name", "arguments", "status"],
                "function_call",
            )?;
            let call_id = required_string(item, "call_id")?;
            identifier(call_id, "call_id")?;
            let name = required_string(item, "name")?;
            identifier(name, "function name")?;
            let arguments = required_string(item, "arguments")?;
            bounded_text(arguments, MAX_TEXT_BYTES, "function arguments")?;
            let parsed: Value =
                serde_json::from_str(arguments).map_err(|_| "Function arguments must be JSON")?;
            if !parsed.is_object() {
                return Err("Function arguments must be a JSON object".into());
            }
            let call = ToolCall {
                id: call_id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: arguments.into(),
                },
            };
            if let Some(previous) = normalized
                .messages
                .last_mut()
                .filter(|message| message.role == "assistant")
            {
                previous.tool_calls.push(call);
            } else {
                let mut message = Message::text("assistant", "");
                message.tool_calls.push(call);
                normalized.messages.push(message);
            }
            Ok(())
        }
        "function_call_output" => {
            object_keys(
                item,
                &["type", "id", "call_id", "output", "status"],
                "function_call_output",
            )?;
            let call_id = required_string(item, "call_id")?;
            identifier(call_id, "call_id")?;
            let output = text_content(
                item.get("output").ok_or("Function output is required")?,
                false,
            )?;
            normalized
                .messages
                .push(Message::tool_result(call_id, &output));
            normalized.tool_outputs.push(ToolOutput {
                call_id: call_id.into(),
                output,
            });
            Ok(())
        }
        _ => Err("Unsupported input item type; use text messages or function call items".into()),
    }
}

fn normalize_message(item: &Value, normalized: &mut NormalizedInput) -> Result<(), String> {
    object_keys(
        item,
        &["type", "id", "role", "content", "status"],
        "message",
    )?;
    let role = required_string(item, "role")?;
    if !matches!(role, "user" | "assistant" | "developer" | "system") {
        return Err("Message role must be user, assistant, developer or system".into());
    }
    let raw_content = item.get("content").ok_or("Message content is required")?;
    if role == "user" {
        let mut message = Message::text(role, "");
        message.content = user_content(raw_content)?;
        normalized.messages.push(message);
        return Ok(());
    }
    let content = text_content(raw_content, role == "assistant")?;
    if matches!(role, "system" | "developer") {
        normalized
            .history_instructions
            .push(Message::text(role, content));
    } else {
        normalized.messages.push(Message::text(role, content));
    }
    Ok(())
}

fn image_detail(detail: Option<&Value>) -> Result<&str, String> {
    match detail {
        None => Ok("auto"),
        Some(Value::String(detail)) if matches!(detail.as_str(), "auto" | "low" | "high") => {
            Ok(detail)
        }
        _ => Err("Image detail must be auto, low or high".into()),
    }
}

fn image_bytes(url: &str) -> Result<usize, String> {
    let (prefix, encoded) = url
        .split_once(',')
        .ok_or("Images must use bounded base64 data URLs or owned Space file IDs")?;
    if !matches!(
        prefix,
        "data:image/png;base64"
            | "data:image/jpeg;base64"
            | "data:image/gif;base64"
            | "data:image/webp;base64"
    ) || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
    {
        return Err(
            "Images must be PNG, JPEG, GIF or WebP base64 data URLs of at most 5 MiB".into(),
        );
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Image data is not valid base64")?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image data must contain between 1 byte and 5 MiB".into());
    }
    Ok(bytes.len())
}

fn user_content(content: &Value) -> Result<Value, String> {
    if content.is_string() {
        return text_content(content, false).map(Value::String);
    }
    let parts = content
        .as_array()
        .ok_or("User content must be text or content parts")?;
    if parts.len() > MAX_INPUT_ITEMS {
        return Err("A message may contain at most 128 content parts".into());
    }
    let mut provider_parts = vec![];
    let mut text_bytes = 0;
    let mut has_image = false;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") => {
                object_keys(part, &["type", "text"], "input_text")?;
                let text = required_string(part, "text")?;
                bounded_text(text, MAX_TEXT_BYTES, "Input text")?;
                text_bytes += text.len();
                if text_bytes > MAX_TEXT_BYTES {
                    return Err("Input text exceeds 256 KiB".into());
                }
                provider_parts.push(json!({"type":"text","text":text}));
            }
            Some("input_image") => {
                object_keys(part, &["type", "image_url", "detail"], "input_image")?;
                let url = required_string(part, "image_url")?;
                image_bytes(url)?;
                let detail = image_detail(part.get("detail"))?;
                provider_parts
                    .push(json!({"type":"image_url","image_url":{"url":url,"detail":detail}}));
                has_image = true;
            }
            Some("input_file") => {
                return Err(
                    "File references must be resolved through the owner's Space before execution"
                        .into(),
                );
            }
            _ => return Err(
                "Unsupported user content part; use text, owned Space files or bounded data images"
                    .into(),
            ),
        }
    }
    if has_image {
        Ok(Value::Array(provider_parts))
    } else {
        Ok(Value::String(
            provider_parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect(),
        ))
    }
}

fn validate_image_budget(messages: &[Message]) -> Result<(), String> {
    let mut count = 0;
    let mut bytes = 0;
    for part in messages
        .iter()
        .filter_map(|message| message.content.as_array())
        .flatten()
    {
        if let Some(url) = part
            .get("image_url")
            .and_then(|image| image.get("url"))
            .and_then(Value::as_str)
        {
            count += 1;
            bytes += image_bytes(url)?;
            if count > MAX_IMAGES || bytes > MAX_TOTAL_IMAGE_BYTES {
                return Err("A response accepts at most 8 images totaling 4 MiB".into());
            }
        }
    }
    Ok(())
}

fn text_content(content: &Value, assistant: bool) -> Result<String, String> {
    if let Some(text) = content.as_str() {
        bounded_text(text, MAX_TEXT_BYTES, "Input text")?;
        return Ok(text.into());
    }
    let parts = content
        .as_array()
        .ok_or("Content must be text or an array of text parts")?;
    if parts.len() > MAX_INPUT_ITEMS {
        return Err("A message may contain at most 128 text parts".into());
    }
    let mut text = String::new();
    for part in parts {
        let kind = part
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Content part type is required")?;
        match kind {
            "input_text" => {
                object_keys(part, &["type", "text"], "input_text")?;
                text.push_str(required_string(part, "text")?);
            }
            "output_text" if assistant => {
                object_keys(part, &["type", "text", "annotations", "logprobs"], "output_text")?;
                text.push_str(required_string(part, "text")?);
            }
            "refusal" if assistant => {
                object_keys(part, &["type", "refusal"], "refusal")?;
                text.push_str(required_string(part, "refusal")?);
            }
            "input_file" => return Err("File references must be resolved through the owner's Space before execution".into()),
            _ => return Err("This endpoint accepts text and Space file inputs; image and audio inputs are not supported".into()),
        }
        bounded_text(&text, MAX_TEXT_BYTES, "Input text")?;
    }
    Ok(text)
}

/// A deterministic public item identity does not depend on the upstream's reused call IDs.
pub fn output_items(message: &Message, id_prefix: &str, status: &str) -> Vec<Value> {
    if message.role != "assistant" {
        return vec![];
    }
    let mut items = vec![];
    let mut content = vec![];
    if let Some(text) = message.content.as_str().filter(|text| !text.is_empty()) {
        content.push(json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]}));
    }
    if let Some(refusal) = message
        .extra
        .get("refusal")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        content.push(json!({"type":"refusal","refusal":refusal}));
    }
    if !content.is_empty() {
        items.push(json!({"type":"message","id":format!("{id_prefix}_message"),
            "role":"assistant","status":status,"content":content}));
    }
    for (index, call) in message.tool_calls.iter().enumerate() {
        items.push(
            json!({"type":"function_call","id":format!("{id_prefix}_call_{index}"),
            "call_id":call.id,"name":call.function.name,"arguments":call.function.arguments,
            "status":status}),
        );
    }
    items
}

pub fn response_value(
    request: &ResponseRequest,
    id: &str,
    created_at: u64,
    status: &str,
    output: &[Value],
    usage: Option<&Usage>,
) -> Value {
    let reported = usage.filter(|usage| usage.reported);
    let usage_value = reported.map(|usage| json!({
        "input_tokens":usage.prompt_tokens,
        "input_tokens_details":{"cached_tokens":usage.cached_tokens},
        "output_tokens":usage.completion_tokens,
        "output_tokens_details":usage.reasoning_tokens.map(|tokens| json!({"reasoning_tokens":tokens})),
        "total_tokens":usage.total_tokens,
    }));
    let mut value = json!({
        "id":id,"object":"response","created_at":created_at,"completed_at":null,
        "status":status,"error":null,"incomplete_details":null,"output":output,
        "model":request.model,"previous_response_id":request.previous_response_id,
        "instructions":request.instructions,"tools":request.tools,
        "tool_choice":request.tool_choice.as_ref().cloned().unwrap_or(json!("auto")),
        "parallel_tool_calls":request.parallel_tool_calls.unwrap_or(true),
        "max_output_tokens":request.max_output_tokens,"temperature":request.temperature,
        "top_p":request.top_p,"text":request.text.as_ref().cloned().unwrap_or(json!({"format":{"type":"text"}})),
        "reasoning":request.reasoning,"metadata":request.metadata,"background":request.background,
        "store":request.store,"truncation":request.truncation.as_deref().unwrap_or("disabled"),
        "usage":null,
    });
    if let (Some(usage), Some(mut usage_value)) = (reported, usage_value) {
        if usage.reasoning_tokens.is_some() {
            value["usage"] = usage_value;
        } else {
            // The standard SDK requires an integer reasoning breakdown. A
            // provider which reports totals alone cannot establish zero: keep
            // the standard object valid and preserve exact known counters in
            // the explicit extension rather than fabricating that breakdown.
            usage_value["output_tokens_details"] = json!({"reasoning_tokens":null});
            value["agents_usage"] = usage_value;
        }
    }
    value
}

/// Event order is durable state, so reconnects can use the same sequence numbers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Events {
    pub next_sequence: u64,
}

impl Events {
    pub fn event(&mut self, kind: &str, fields: Value) -> Value {
        let mut event = fields.as_object().cloned().unwrap_or_default();
        event.insert("type".into(), json!(kind));
        event.insert("sequence_number".into(), json!(self.next_sequence));
        self.next_sequence = self.next_sequence.saturating_add(1);
        Value::Object(event)
    }

    pub fn lifecycle(&mut self, kind: &str, response: &Value) -> Value {
        self.event(kind, json!({"response":response}))
    }

    pub fn output_item_added(&mut self, index: usize, item: &Value) -> Value {
        self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        )
    }

    pub fn output_item_done(&mut self, index: usize, item: &Value) -> Value {
        self.event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        )
    }

    pub fn content_part_added(
        &mut self,
        item_id: &str,
        output_index: usize,
        content_index: usize,
        part: &Value,
    ) -> Value {
        self.event("response.content_part.added", json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"part":part}))
    }

    pub fn content_part_done(
        &mut self,
        item_id: &str,
        output_index: usize,
        content_index: usize,
        part: &Value,
    ) -> Value {
        self.event("response.content_part.done", json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"part":part}))
    }

    pub fn text_delta(
        &mut self,
        item_id: &str,
        output_index: usize,
        content_index: usize,
        delta: &str,
    ) -> Value {
        self.event("response.output_text.delta", json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"delta":delta,"logprobs":[]}))
    }

    pub fn text_done(
        &mut self,
        item_id: &str,
        output_index: usize,
        content_index: usize,
        text: &str,
    ) -> Value {
        self.event("response.output_text.done", json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"text":text,"logprobs":[]}))
    }

    pub fn function_arguments_delta(
        &mut self,
        item_id: &str,
        output_index: usize,
        delta: &str,
    ) -> Value {
        self.event(
            "response.function_call_arguments.delta",
            json!({"item_id":item_id,"output_index":output_index,"delta":delta}),
        )
    }

    pub fn function_arguments_done(
        &mut self,
        item_id: &str,
        output_index: usize,
        name: &str,
        arguments: &str,
    ) -> Value {
        self.event("response.function_call_arguments.done", json!({"item_id":item_id,"output_index":output_index,"name":name,"arguments":arguments}))
    }
}

pub fn event_frame(event: &Value) -> Vec<u8> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("error");
    format!("event: {kind}\ndata: {event}\n\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(input: Value) -> ResponseRequest {
        serde_json::from_value(json!({"model":"agent_test","input":input})).unwrap()
    }

    #[test]
    fn strings_and_typed_text_have_the_same_provider_input() {
        let string = request(json!("hello 世界")).normalize().unwrap();
        let typed =
            request(json!([{"role":"user","content":[{"type":"input_text","text":"hello 世界"}]}]))
                .normalize()
                .unwrap();
        assert_eq!(
            serde_json::to_value(string.messages).unwrap(),
            serde_json::to_value(typed.messages).unwrap()
        );
    }

    #[test]
    fn easy_input_items_are_canonical_and_have_response_bound_identities() {
        let normalized = request(json!("hello")).normalize().unwrap();
        let identified = identified_input_items(&normalized.input_items, "resp_one");
        assert_eq!(identified[0]["type"], "message");
        assert_eq!(identified[0]["status"], "completed");
        assert_eq!(identified[0]["content"][0]["type"], "input_text");
        assert_eq!(identified[0]["content"][0]["text"], "hello");
        assert_eq!(identified[0]["id"], "resp_one_input_0");
        assert_eq!(
            identified,
            identified_input_items(&normalized.input_items, "resp_one")
        );
    }

    #[test]
    fn early_structure_validation_accepts_owned_file_shapes_not_arbitrary_sources() {
        for content in [
            json!([{"type":"input_file","file_id":"file_one"}]),
            json!([{"type":"input_image","file_id":"file_one","detail":"low"}]),
        ] {
            let request = request(json!([{"role":"user","content":content}]));
            assert!(request.validate_input_structure().is_ok());
            assert!(request.normalize().is_err());
            let structural = request.normalize_structure().unwrap();
            assert_eq!(
                structural.input_items[0]["content"][0]["file_id"],
                "file_one"
            );
            assert_eq!(structural.messages[0].content, "");
        }
        for content in [
            json!([{"type":"input_file","file_url":"https://example.invalid/file"}]),
            json!([{"type":"input_file","file_id":"file_one","file_data":"ignored"}]),
            json!([{"type":"input_image","file_id":"file_one","image_url":"https://example.invalid/a.png"}]),
            json!([{"type":"input_image","file_id":"file_one","detail":"impossible"}]),
        ] {
            assert!(
                request(json!([{"role":"user","content":content}]))
                    .validate_input_structure()
                    .is_err()
            );
        }
        let too_many = vec![json!({"type":"input_image","file_id":"file_one"}); MAX_IMAGES + 1];
        assert!(
            request(json!([{"role":"user","content":too_many}]))
                .validate_input_structure()
                .is_err()
        );
    }

    #[test]
    fn native_images_preserve_content_order_and_never_fetch_remote_urls() {
        let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5n8AAAAASUVORK5CYII=";
        let normalized = request(json!([{"role":"user","content":[
            {"type":"input_text","text":"Before"},
            {"type":"input_image","image_url":url,"detail":"low"},
            {"type":"input_text","text":"After"}
        ]}]))
        .normalize()
        .unwrap();
        let parts = normalized.messages[0].content.as_array().unwrap();
        assert_eq!(parts[0], json!({"type":"text","text":"Before"}));
        assert_eq!(
            parts[1],
            json!({"type":"image_url","image_url":{"url":url,"detail":"low"}})
        );
        assert_eq!(parts[2], json!({"type":"text","text":"After"}));
        for url in [
            "https://example.invalid/a.png",
            "file:///a.png",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64,!!!!",
        ] {
            assert!(
                request(
                    json!([{"role":"user","content":[{"type":"input_image","image_url":url}]}])
                )
                .normalize()
                .is_err()
            );
        }
        let too_many = vec![json!({"type":"input_image","image_url":url}); MAX_IMAGES + 1];
        assert!(
            request(json!([{"role":"user","content":too_many}]))
                .normalize()
                .is_err()
        );
    }

    #[test]
    fn request_schemas_are_compiled_before_execution() {
        let mut request = request(json!("hello"));
        request.tools = serde_json::from_value(json!([{"type":"function","name":"local","parameters":{"type":"object","properties":{"count":{"minimum":"wrong"}}}}])).unwrap();
        assert!(request.validate().is_err());
        request.tools.clear();
        request.text = Some(
            json!({"format":{"type":"json_schema","name":"result","schema":{"$ref":"https://example.invalid/schema"}}}),
        );
        assert!(request.validate().is_err());
    }

    #[test]
    fn custom_instructions_are_separate_from_history() {
        let mut request = request(json!([
            {"role":"system","content":"First custom constraint"},
            {"role":"developer","content":[{"type":"input_text","text":"Second constraint"}]},
            {"role":"user","content":"hello"}
        ]));
        request.instructions = Some("Request instruction".into());
        let normalized = request.normalize().unwrap();
        assert_eq!(normalized.instructions, ["Request instruction"]);
        assert_eq!(normalized.history_instructions.len(), 2);
        assert_eq!(normalized.history_instructions[0].role, "system");
        assert_eq!(
            normalized.history_instructions[0].content,
            "First custom constraint"
        );
        assert_eq!(normalized.history_instructions[1].role, "developer");
        assert_eq!(
            normalized.history_instructions[1].content,
            "Second constraint"
        );
        assert_eq!(normalized.messages.len(), 1);
        assert_eq!(normalized.input_items.len(), 3);
    }

    #[test]
    fn function_calls_round_trip_flat_items_and_correlated_outputs() {
        let normalized = request(json!([
            {"type":"function_call","id":"fc_one","call_id":"call_one","name":"read_local","arguments":"{\"path\":\"a\"}"},
            {"type":"function_call","call_id":"call_two","name":"read_local","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_one","output":"contents"},
            {"type":"function_call_output","call_id":"call_two","output":[{"type":"input_text","text":"more"}]}
        ])).normalize().unwrap();
        assert_eq!(normalized.messages.len(), 3);
        assert_eq!(normalized.messages[0].tool_calls.len(), 2);
        assert_eq!(
            normalized.messages[1].tool_call_id.as_deref(),
            Some("call_one")
        );
        assert_eq!(
            normalized.tool_outputs,
            [
                ToolOutput {
                    call_id: "call_one".into(),
                    output: "contents".into()
                },
                ToolOutput {
                    call_id: "call_two".into(),
                    output: "more".into()
                }
            ]
        );
        let output = output_items(&normalized.messages[0], "resp_one", "completed");
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_one");
        assert_ne!(output[0]["id"], output[1]["id"]);
    }

    #[test]
    fn duplicate_outputs_and_unsupported_input_fail_before_execution() {
        for input in [
            json!([{"type":"function_call_output","call_id":"call_one","output":"a"},{"type":"function_call_output","call_id":"call_one","output":"b"}]),
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/a.png"}]}]),
            json!([{"type":"item_reference","id":"item_old"}]),
            json!([{"role":"tool","content":"fake"}]),
            json!([{"role":"user","content":"a","surprise":true}]),
        ] {
            assert!(request(input).normalize().is_err());
        }
    }

    #[test]
    fn flat_tools_validate_unique_names_and_object_schemas() {
        let mut request = request(json!("hello"));
        request.tools = serde_json::from_value(json!([{"type":"function","name":"read_local","parameters":{"type":"object","properties":{"path":{"type":"string"}}},"strict":false}])).unwrap();
        let definitions = request.tool_definitions().unwrap();
        assert_eq!(definitions[0].function.name, "read_local");
        request.tools.push(request.tools[0].clone());
        assert!(request.validate().is_err());
        request.tools.pop();
        request.tools[0].parameters = json!({"type":"array"});
        assert!(request.validate().is_err());
    }

    #[test]
    fn options_are_bounded_and_unknown_options_are_not_silently_ignored() {
        for options in [
            json!({"temperature":2.1}),
            json!({"top_p":-0.1}),
            json!({"max_output_tokens":0}),
            json!({"future_feature":true}),
            json!({"background":true,"store":false}),
            json!({"stream_options":{"include_obfuscation":false}}),
            json!({"text":{"format":{"type":"made_up"}}}),
            json!({"text":{"format":{"type":"json_schema","name":"named","schema":true}}}),
            json!({"include":["reasoning.encrypted_content"]}),
        ] {
            let mut value = json!({"model":"agent_test","input":"hello"});
            value
                .as_object_mut()
                .unwrap()
                .extend(options.as_object().unwrap().clone());
            let parsed: ResponseRequest = serde_json::from_value(value).unwrap();
            assert!(parsed.validate().is_err());
        }
        let mut request = request(json!("hello"));
        request.text = Some(
            json!({"format":{"type":"json_schema","name":"Result","schema":{"type":"object"},"strict":true}}),
        );
        request.stream = true;
        request.stream_options = Some(json!({"include_obfuscation":false}));
        assert!(request.validate().is_ok());
        assert_eq!(
            request.generation_options().text_format.unwrap()["name"],
            "Result"
        );
    }

    #[test]
    fn response_conversion_never_leaks_provider_continuation_fields() {
        let mut message = Message::text("assistant", "public answer");
        message
            .extra
            .insert("reasoning_content".into(), json!("private continuation"));
        message.extra.insert(
            "reasoning_details".into(),
            json!([{"signature":"private signature"}]),
        );
        let items = output_items(&message, "resp_one", "completed");
        assert_eq!(items[0]["content"][0]["text"], "public answer");
        assert!(!serde_json::to_string(&items).unwrap().contains("private"));
        let value = response_value(
            &request(json!("hello")),
            "resp_one",
            123,
            "completed",
            &items,
            None,
        );
        assert_eq!(value["object"], "response");
        assert!(value["usage"].is_null());
        assert!(value["completed_at"].is_null());
    }

    #[test]
    fn usage_requires_an_authoritative_reasoning_breakdown_for_the_standard_shape() {
        let mut usage = Usage {
            prompt_tokens: 12,
            completion_tokens: 4,
            total_tokens: 16,
            cached_tokens: 3,
            reported: true,
            ..Usage::default()
        };
        let request = request(json!("hello"));
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert!(value["usage"].is_null());
        assert_eq!(value["agents_usage"]["input_tokens"], 12);
        assert_eq!(value["agents_usage"]["output_tokens"], 4);
        assert_eq!(value["agents_usage"]["total_tokens"], 16);
        assert_eq!(
            value["agents_usage"]["input_tokens_details"]["cached_tokens"],
            3
        );
        assert!(value["agents_usage"]["output_tokens_details"]["reasoning_tokens"].is_null());
        usage.reasoning_tokens = Some(2);
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
        assert!(value.get("agents_usage").is_none());
        usage.reasoning_tokens = Some(0);
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
        usage.reported = false;
        let value = response_value(&request, "resp_one", 1, "failed", &[], Some(&usage));
        assert!(value["usage"].is_null());
        assert!(value.get("agents_usage").is_none());
    }

    #[test]
    fn events_follow_sdk_indexes_and_sse_framing() {
        let mut events = Events::default();
        let created = events.lifecycle("response.created", &json!({"id":"resp_one"}));
        let delta = events.text_delta("msg_one", 0, 0, "hello\n世界");
        let done = events.text_done("msg_one", 0, 0, "hello\n世界");
        assert_eq!(created["sequence_number"], 0);
        assert_eq!(delta["sequence_number"], 1);
        assert_eq!(done["sequence_number"], 2);
        assert_eq!(delta["content_index"], 0);
        let frame = String::from_utf8(event_frame(&delta)).unwrap();
        assert!(frame.starts_with("event: response.output_text.delta\ndata: {"));
        assert!(frame.ends_with("\n\n"));
        assert_eq!(frame.matches('\n').count(), 3);
    }

    proptest! {
        #[test]
        fn text_normalization_preserves_all_unicode(text in "[^\\x00]{0,256}") {
            let normalized = request(json!(text.clone())).normalize().unwrap();
            prop_assert_eq!(normalized.messages[0].content.as_str(), Some(text.as_str()));
        }

        #[test]
        fn event_sequences_resume_monotonically(start in 0u64..u64::MAX-128, count in 1usize..128) {
            let mut events = Events { next_sequence:start };
            for index in 0..count {
                let event = events.event("test", json!({"sequence_number":0,"type":"overwrite"}));
                prop_assert_eq!(event["sequence_number"].as_u64(), Some(start + index as u64));
                prop_assert_eq!(event["type"].as_str(), Some("test"));
            }
            let restored: Events = serde_json::from_value(serde_json::to_value(&events).unwrap()).unwrap();
            prop_assert_eq!(restored.next_sequence, start + count as u64);
        }
    }
}
