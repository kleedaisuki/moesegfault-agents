//! Responses wire objects are independent of durable execution and provider routing.
//!
//! The public output is deliberately not a serialization of a provider message:
//! private reasoning continuations and provider-specific fields stay in the journal.
//! Contract: https://developers.openai.com/api/reference/resources/responses

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::{
    provider::{FunctionCall, GenerationOptions, Message, ToolCall, Usage},
    tools::ToolDefinition,
};

const MAX_INPUT_ITEMS: usize = 128;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGES: usize = 8;

#[path = "responses_controls.rs"]
mod controls;
#[path = "responses_retrieval.rs"]
mod retrieval;
pub use retrieval::{ResponseProjection, RetrieveOptions};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub conversation: Option<Value>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub tools: Vec<ResponseTool>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub stream: bool,
    #[serde(default, deserialize_with = "nullable_default")]
    pub background: bool,
    #[serde(default = "yes", deserialize_with = "nullable_true")]
    pub store: bool,
    #[serde(default, deserialize_with = "nullable_default")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
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
    #[serde(default, deserialize_with = "nullable_default")]
    pub include: Vec<String>,
    #[serde(default)]
    pub stream_options: Option<Value>,
    #[serde(default)]
    pub context_management: Option<Value>,
    #[serde(default)]
    pub prompt: Option<Value>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub prompt_cache_options: Option<Value>,
    #[serde(default)]
    pub prompt_cache_retention: Option<String>,
    #[serde(default)]
    pub top_logprobs: Option<u8>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub safety_identifier: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub moderation: Option<Value>,
    #[serde(default)]
    pub access_programs: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone)]
pub struct ResponseTool {
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: Option<bool>,
    pub options: BTreeMap<String, Value>,
}

impl std::fmt::Debug for ResponseTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResponseTool")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for ResponseTool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let kind = fields
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| serde::de::Error::custom("tool type is required"))?;
        let name = fields
            .remove("name")
            .map(serde_json::from_value::<String>)
            .transpose()
            .map_err(serde::de::Error::custom)?
            .unwrap_or_default();
        let description = fields
            .remove("description")
            .map(serde_json::from_value::<Option<String>>)
            .transpose()
            .map_err(serde::de::Error::custom)?
            .flatten();
        let parameters = if kind == "function" {
            fields.remove("parameters").unwrap_or_else(empty_schema)
        } else {
            empty_schema()
        };
        let strict = if kind == "function" {
            fields
                .remove("strict")
                .map(serde_json::from_value::<Option<bool>>)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .flatten()
        } else {
            None
        };
        Ok(Self {
            kind,
            name,
            description,
            parameters,
            strict,
            options: fields,
        })
    }
}

impl Serialize for ResponseTool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire_value().serialize(serializer)
    }
}

impl ResponseTool {
    pub fn to_wire_value(&self) -> Value {
        let mut fields = self.options.clone();
        fields.insert("type".into(), json!(self.kind));
        if matches!(self.kind.as_str(), "function" | "custom" | "namespace") {
            fields.insert("name".into(), json!(self.name));
        }
        if let Some(description) = &self.description {
            fields.insert("description".into(), json!(description));
        }
        if self.kind == "function" {
            fields.insert("parameters".into(), self.parameters.clone());
            if let Some(strict) = self.strict {
                fields.insert("strict".into(), json!(strict));
            }
        }
        serde_json::to_value(fields).expect("JSON tool fields")
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn public_value(&self) -> Value {
        let mut value = self.to_wire_value();
        if self.kind == "mcp"
            && let Some(fields) = value.as_object_mut()
        {
            fields.remove("authorization");
            fields.remove("headers");
        }
        let policy_path = match self.kind.as_str() {
            "code_interpreter" => "/container/network_policy",
            "shell" => "/environment/network_policy",
            _ => return value,
        };
        if let Some(policy) = value
            .pointer_mut(policy_path)
            .and_then(Value::as_object_mut)
        {
            // The optional secret declarations are execution input, never a
            // public tool descriptor (including on rejected or resumed runs).
            policy.remove("domain_secrets");
        }
        value
    }

    pub fn code_network_policy(&self) -> Result<Option<CodeNetworkPolicy>, String> {
        if self.kind != "code_interpreter" {
            return Ok(None);
        }
        self.options
            .get("container")
            .and_then(|container| container.get("network_policy"))
            .map(|policy| {
                serde_json::from_value(policy.clone())
                    .map_err(|_| "Invalid code interpreter network_policy".to_owned())
            })
            .transpose()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodeNetworkPolicy {
    Disabled {},
    Allowlist {
        allowed_domains: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        domain_secrets: Vec<DomainSecret>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainSecret {
    pub domain: String,
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for DomainSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DomainSecret { .. }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub call_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct NormalizedInput {
    pub messages: Vec<Message>,
    pub instructions: Vec<String>,
    pub history_instructions: Vec<Message>,
    pub input_items: Vec<Value>,
    pub tool_outputs: Vec<ToolOutput>,
    pub native_items: Vec<Value>,
    pub native_only: bool,
}

fn nullable_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn nullable_true<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Option::<bool>::deserialize(deserializer).map(|value| value.unwrap_or(true))
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
    pub fn validate_resolved_tool_choice(
        &self,
        definitions: &[ToolDefinition],
    ) -> Result<(), String> {
        if let Some(choice) = &self.tool_choice {
            controls::resolved_tool_choice(choice, &self.tools, definitions)?;
        }
        Ok(())
    }

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
        controls::validate(self)?;
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
        let mut schemas = crate::responses_schema::SchemaBudget::default();
        self.validate_tools(&mut schemas)?;
        self.validate_text(&mut schemas)?;
        Ok(())
    }

    fn validate_tools(
        &self,
        schemas: &mut crate::responses_schema::SchemaBudget,
    ) -> Result<(), String> {
        if self.tools.len() > 128 {
            return Err("At most 128 client tools may be registered".into());
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if tool.kind != "function" {
                controls::builtin_tool(tool, schemas)?;
            } else {
                validate_function(tool, schemas, 64)?;
            }
            if !tool.name.is_empty() && !names.insert((tool.kind.as_str(), tool.name.as_str())) {
                return Err("Named tool definitions must be unique within their type".into());
            }
            if let Some(description) = &tool.description {
                bounded_text(description, MAX_INSTRUCTIONS_BYTES, "function description")?;
            }
        }
        Ok(())
    }

    fn validate_text(
        &self,
        schemas: &mut crate::responses_schema::SchemaBudget,
    ) -> Result<(), String> {
        let Some(text) = &self.text else {
            return Ok(());
        };
        object_keys(text, &["format", "verbosity"], "text")?;
        if text.get("verbosity").is_some_and(|value| {
            !value.is_null() && !matches!(value.as_str(), Some("low" | "medium" | "high"))
        }) {
            return Err("text.verbosity must be low, medium or high".into());
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
                schemas.validate(&format["schema"])?;
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
            ..GenerationOptions::default()
        }
    }

    pub fn tool_definitions(&self) -> Result<Vec<ToolDefinition>, String> {
        if self.tools.iter().any(|tool| tool.kind != "function") {
            return Err("Non-function tools require resolved native or hosted bindings".into());
        }
        self.function_definitions()
    }

    /// Function projection for callers which separately resolve all typed tools.
    pub fn function_definitions(&self) -> Result<Vec<ToolDefinition>, String> {
        self.validate_tools(&mut crate::responses_schema::SchemaBudget::default())?;
        self.tools
            .iter()
            .filter(|tool| tool.kind == "function")
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

    pub fn native_tool_items(&self) -> Result<Vec<Value>, String> {
        self.validate_tools(&mut crate::responses_schema::SchemaBudget::default())?;
        Ok(self.tools.iter().map(ResponseTool::to_wire_value).collect())
    }

    pub fn function_tools(&self) -> impl Iterator<Item = &ResponseTool> {
        self.tools.iter().filter(|tool| tool.kind == "function")
    }

    pub fn native_input_items(&self) -> Result<Vec<Value>, String> {
        self.normalize_structure()
            .map(|normalized| normalized.native_items)
    }

    pub fn conversation_id(&self) -> Result<Option<&str>, String> {
        controls::validate(self)?;
        Ok(self.conversation.as_ref().and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
        }))
    }

    pub fn native_controls(&self) -> Value {
        let mut controls = json!({
            "max_output_tokens":self.max_output_tokens,"max_tool_calls":self.max_tool_calls,
            "temperature":self.temperature,"top_p":self.top_p,"parallel_tool_calls":self.parallel_tool_calls,
            "tool_choice":self.tool_choice,"text":self.text,"reasoning":self.reasoning,
            "truncation":self.truncation,"include":self.include,"top_logprobs":self.top_logprobs,
            "prompt_cache_key":self.prompt_cache_key,"prompt_cache_options":self.prompt_cache_options,
            "prompt_cache_retention":self.prompt_cache_retention,"service_tier":self.service_tier,
            "safety_identifier":self.safety_identifier,"user":self.user,"moderation":self.moderation,
            "access_programs":self.access_programs,
        });
        controls
            .as_object_mut()
            .unwrap()
            .retain(|_, value| !value.is_null());
        controls
    }

    pub fn requires_native_provider(&self) -> bool {
        !self.include.is_empty()
            || self.top_logprobs.is_some()
            || self.prompt.is_some()
            || self.prompt_cache_key.is_some()
            || self.prompt_cache_options.is_some()
            || self.prompt_cache_retention.is_some()
            || self.service_tier.is_some()
            || self.safety_identifier.is_some()
            || self.user.is_some()
            || self.moderation.is_some()
            || self.access_programs.is_some()
            || self
                .text
                .as_ref()
                .is_some_and(|value| value.get("verbosity").is_some())
            || self.reasoning.as_ref().is_some_and(|value| {
                value
                    .as_object()
                    .is_some_and(|fields| fields.keys().any(|name| name != "effort"))
            })
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
        let mut bytes = 0;
        placeholder_files(&mut input, &mut images, &mut bytes)?;
        let mut normalized = self.normalize_input(&input)?;
        // Public input history keeps owner-scoped references, not a second copy
        // of bytes already durably stored in Space.
        normalized.input_items = input_items(&self.input)?;
        normalized.native_items = raw_input_items(&self.input)?;
        normalized.native_only |= normalized.native_items.iter().any(native_features);
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
            normalized.native_only |= native_features(&item);
            normalized.input_items.push(canonical_input_item(&item)?);
            normalized.native_items.push(item);
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

fn validate_function(
    tool: &ResponseTool,
    schemas: &mut crate::responses_schema::SchemaBudget,
    name_limit: usize,
) -> Result<(), String> {
    if tool.name.is_empty()
        || tool.name.len() > name_limit
        || !tool
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(format!(
            "Function names must contain 1 to {name_limit} ASCII letters, digits, underscores or hyphens"
        ));
    }
    if tool.options.keys().any(|key| {
        !matches!(
            key.as_str(),
            "allowed_callers" | "async" | "defer_loading" | "output_schema"
        )
    }) {
        return Err("Unknown function tool field".into());
    }
    controls::callers(&tool.to_wire_value())?;
    for key in ["async", "defer_loading"] {
        if tool
            .options
            .get(key)
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(format!("{key} must be boolean"));
        }
    }
    if !tool.parameters.is_null()
        && (!tool.parameters.is_object()
            || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
            || tool
                .parameters
                .get("properties")
                .is_some_and(|value| !value.is_object()))
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
    if !tool.parameters.is_null() {
        schemas.validate(&tool.parameters)?;
    }
    if let Some(output) = tool
        .options
        .get("output_schema")
        .filter(|value| !value.is_null())
    {
        schemas.validate(output)?;
    }
    Ok(())
}

fn input_items(input: &Value) -> Result<Vec<Value>, String> {
    raw_input_items(input)?
        .iter()
        .map(canonical_input_item)
        .collect()
}

fn placeholder_files(
    value: &mut Value,
    images: &mut usize,
    bytes: &mut usize,
) -> Result<(), String> {
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "input_file" | "input_image"))
    {
        validate_attachment_syntax(value)?;
        if remote_attachment(value) {
            *images += usize::from(value["type"] == "input_image");
            if *images > MAX_IMAGES {
                return Err("A response accepts at most 8 images".into());
            }
            *value = json!({"type":"input_text","text":""});
            return Ok(());
        }
        let mut legacy = value.clone();
        if let Some(fields) = legacy.as_object_mut() {
            fields.remove("prompt_cache_breakpoint");
        }
        if legacy["detail"] == "original" {
            legacy["detail"] = json!("high");
        }
        let (count, size) = crate::responses_attachments::validate_part(&legacy)
            .map_err(|_| "Invalid file or image attachment")?;
        *images += count;
        *bytes += size;
        if *images > MAX_IMAGES || *bytes > MAX_TOTAL_IMAGE_BYTES {
            return Err(
                "A response accepts at most 8 images and 4 MiB of inline attachments".into(),
            );
        }
        *value = json!({"type":"input_text","text":""});
        return Ok(());
    }
    match value {
        Value::Object(object) => {
            for value in object.values_mut() {
                placeholder_files(value, images, bytes)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                placeholder_files(value, images, bytes)?;
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
    if matches!(
        kind.as_str(),
        "message"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
    ) && item.get("status").is_none()
    {
        item["status"] = json!("completed");
    }
    if kind == "message"
        && let Some(text) = item["content"].as_str()
    {
        item["content"] = if item["role"] == "assistant" {
            json!([{"type":"output_text","text":text,"annotations":[],"logprobs":[]}])
        } else {
            json!([{"type":"input_text","text":text}])
        };
    }
    let content_key = if kind == "function_call_output" {
        "output"
    } else {
        "content"
    };
    if let Some(parts) = item.get_mut(content_key).and_then(Value::as_array_mut) {
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

fn remote_attachment(part: &Value) -> bool {
    part.get("file_url").is_some()
        || part
            .get("image_url")
            .and_then(Value::as_str)
            .is_some_and(|url| !url.starts_with("data:"))
}

fn public_url(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "Attachment URL is invalid")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.host_str().is_some_and(public_attachment_host)
    {
        return Err(
            "Attachments require a public HTTPS URL without credentials or fragment".into(),
        );
    }
    bounded_text(value, 8192, "attachment URL")
}

fn public_attachment_host(host: &str) -> bool {
    // This is syntax policy, not DNS authorization; remote freezing must also
    // validate resolved addresses and every redirect before fetching bytes.
    let host = host.trim_end_matches('.');
    host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && !host.starts_with('[')
        && ![
            "localhost",
            "local",
            "internal",
            "home",
            "lan",
            "test",
            "invalid",
        ]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        && !matches!(host, "metadata.google.internal" | "metadata.google")
}

fn validate_attachment_syntax(part: &Value) -> Result<(), String> {
    let image = part["type"] == "input_image";
    let keys: &[&str] = if image {
        &[
            "type",
            "detail",
            "file_id",
            "image_url",
            "prompt_cache_breakpoint",
        ]
    } else {
        &[
            "type",
            "detail",
            "file_id",
            "file_data",
            "file_url",
            "filename",
            "prompt_cache_breakpoint",
        ]
    };
    object_keys(part, keys, "attachment")?;
    let sources = if image {
        &["file_id", "image_url"][..]
    } else {
        &["file_id", "file_data", "file_url"][..]
    };
    if sources
        .iter()
        .filter(|key| part.get(**key).is_some_and(|value| !value.is_null()))
        .count()
        != 1
    {
        return Err("An attachment requires exactly one content source".into());
    }
    for key in sources {
        if let Some(value) = part.get(*key).filter(|value| !value.is_null()) {
            let value = value.as_str().ok_or("Attachment source must be text")?;
            if *key == "file_id" {
                identifier(value, "file ID")?;
            }
            if *key == "file_url" || (*key == "image_url" && !value.starts_with("data:")) {
                public_url(value)?;
            }
        }
    }
    if let Some(detail) = part.get("detail").filter(|value| !value.is_null()) {
        if !matches!(detail.as_str(), Some("auto" | "low" | "high" | "original")) {
            return Err("Invalid attachment detail".into());
        }
    }
    if let Some(filename) = part.get("filename") {
        bounded_text(
            filename.as_str().ok_or("filename must be text")?,
            512,
            "filename",
        )?;
    }
    cache_breakpoint(part)
}

fn cache_breakpoint(part: &Value) -> Result<(), String> {
    if let Some(value) = part.get("prompt_cache_breakpoint") {
        object_keys(value, &["mode"], "prompt_cache_breakpoint")?;
        if value["mode"] != "explicit" {
            return Err("Only explicit content cache breakpoints are supported".into());
        }
    }
    Ok(())
}

fn rich_content(content: &Value) -> bool {
    content.as_array().is_some_and(|parts| {
        parts.iter().any(|part| {
            part.get("prompt_cache_breakpoint").is_some()
                || remote_attachment(part)
                || part["detail"] == "original"
                || part["type"] == "refusal"
        })
    })
}

fn native_features(item: &Value) -> bool {
    !matches!(
        item.get("type")
            .and_then(Value::as_str)
            .unwrap_or("message"),
        "message" | "function_call" | "function_call_output"
    ) || item.get("phase").is_some()
        || rich_content(&item["content"])
        || rich_content(&item["output"])
        || item.get("namespace").is_some()
        || item.get("caller").is_some()
}

fn validate_native_content(content: &Value, assistant: bool) -> Result<(), String> {
    let parts = content
        .as_array()
        .filter(|parts| parts.len() <= MAX_INPUT_ITEMS)
        .ok_or("Content must be a bounded parts array")?;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") => {
                object_keys(
                    part,
                    &["type", "text", "prompt_cache_breakpoint"],
                    "input_text",
                )?;
                bounded_text(required_string(part, "text")?, MAX_TEXT_BYTES, "input text")?;
                cache_breakpoint(part)?;
            }
            Some("output_text") if assistant => {
                object_keys(
                    part,
                    &["type", "text", "annotations", "logprobs"],
                    "output_text",
                )?;
                bounded_text(
                    required_string(part, "text")?,
                    MAX_TEXT_BYTES,
                    "output text",
                )?;
            }
            Some("refusal") if assistant => {
                object_keys(part, &["type", "refusal"], "refusal")?;
                bounded_text(required_string(part, "refusal")?, MAX_TEXT_BYTES, "refusal")?;
            }
            Some("input_image" | "input_file") => validate_attachment_syntax(part)?,
            _ => return Err("Unknown message content part".into()),
        }
    }
    Ok(())
}

fn validate_native_item(item: &Value) -> Result<(), String> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or("Input item requires a type")?;
    if !item.is_object() {
        return Err("Input items must be objects".into());
    }
    if serde_json::to_vec(item)
        .map_err(|_| "Invalid native input")?
        .len()
        > 4 * 1024 * 1024
    {
        return Err("Native item exceeds 4 MiB".into());
    }
    if let Some(id) = item.get("id").filter(|value| !value.is_null()) {
        identifier(id.as_str().ok_or("Item ID must be text")?, "item ID")?;
    }
    match kind {
        "item_reference" => {
            object_keys(item, &["type", "id"], "item_reference")?;
            identifier(required_string(item, "id")?, "item reference")?;
        }
        "reasoning" => {
            object_keys(
                item,
                &[
                    "type",
                    "id",
                    "summary",
                    "content",
                    "encrypted_content",
                    "status",
                ],
                "reasoning item",
            )?;
            if !item.get("summary").is_some_and(Value::is_array) {
                return Err("Reasoning item requires summary parts".into());
            }
        }
        "compaction" => {
            object_keys(
                item,
                &["type", "id", "encrypted_content"],
                "compaction item",
            )?;
            bounded_text(
                required_string(item, "encrypted_content")?,
                4 * 1024 * 1024,
                "compaction state",
            )?;
        }
        "custom_tool_call" => {
            object_keys(
                item,
                &[
                    "type",
                    "id",
                    "call_id",
                    "name",
                    "input",
                    "status",
                    "namespace",
                    "caller",
                ],
                "custom call",
            )?;
            identifier(required_string(item, "call_id")?, "call ID")?;
            identifier(required_string(item, "name")?, "custom tool name")?;
            bounded_text(
                required_string(item, "input")?,
                MAX_TEXT_BYTES,
                "custom input",
            )?;
        }
        "custom_tool_call_output" => {
            object_keys(
                item,
                &["type", "id", "call_id", "output", "status"],
                "custom output",
            )?;
            identifier(required_string(item, "call_id")?, "call ID")?;
            if !item
                .get("output")
                .is_some_and(|value| value.is_string() || value.is_array())
            {
                return Err("Custom tool output must be text or content parts".into());
            }
        }
        "mcp_approval_response" => {
            object_keys(
                item,
                &["type", "id", "approval_request_id", "approve", "reason"],
                "MCP approval",
            )?;
            identifier(
                required_string(item, "approval_request_id")?,
                "approval request ID",
            )?;
            if !item["approve"].is_boolean() {
                return Err("MCP approval requires a boolean decision".into());
            }
        }
        "computer_call_output"
        | "local_shell_call_output"
        | "shell_call_output"
        | "apply_patch_call_output" => {
            crate::client_tool_items::validate_output(item).map_err(str::to_owned)?;
        }
        "file_search_call"
        | "web_search_call"
        | "computer_call"
        | "image_generation_call"
        | "code_interpreter_call"
        | "local_shell_call"
        | "shell_call"
        | "apply_patch_call"
        | "mcp_list_tools"
        | "mcp_approval_request"
        | "mcp_call"
        | "tool_search_call"
        | "tool_search_output"
        | "compaction_trigger"
        | "additional_tools"
        | "configuration" => {
            // These retain their full typed payload. Capability-specific execution
            // and item-reference ownership are resolved before admitting model work.
        }
        _ => return Err("Unknown Responses input item type".into()),
    }
    Ok(())
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
                &[
                    "type",
                    "id",
                    "call_id",
                    "name",
                    "arguments",
                    "status",
                    "namespace",
                    "caller",
                ],
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
                &["type", "id", "call_id", "output", "status", "caller"],
                "function_call_output",
            )?;
            let call_id = required_string(item, "call_id")?;
            identifier(call_id, "call_id")?;
            let mut message = Message::tool_result(call_id, "");
            let output = item.get("output").ok_or("Function output is required")?;
            if rich_content(output) {
                validate_native_content(output, false)?;
                normalized.native_only = true;
                message.content = json!(
                    output
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>()
                );
            } else {
                message.content = user_content(output)?;
            }
            normalized.messages.push(message);
            normalized.tool_outputs.push(ToolOutput {
                call_id: call_id.into(),
            });
            Ok(())
        }
        _ => {
            validate_native_item(item)?;
            normalized.native_only = true;
            if kind.ends_with("_output")
                && let Some(call_id) = if crate::client_tool_items::is_output(item) {
                    crate::client_tool_items::result_id(item)
                } else {
                    item.get("call_id").and_then(Value::as_str)
                }
            {
                normalized.tool_outputs.push(ToolOutput {
                    call_id: call_id.into(),
                });
            }
            Ok(())
        }
    }
}

fn normalize_message(item: &Value, normalized: &mut NormalizedInput) -> Result<(), String> {
    object_keys(
        item,
        &["type", "id", "role", "content", "status", "phase"],
        "message",
    )?;
    let role = required_string(item, "role")?;
    if !matches!(role, "user" | "assistant" | "developer" | "system") {
        return Err("Message role must be user, assistant, developer or system".into());
    }
    let raw_content = item.get("content").ok_or("Message content is required")?;
    if let Some(phase) = item.get("phase").filter(|value| !value.is_null()) {
        if role != "assistant" || !matches!(phase.as_str(), Some("commentary" | "final_answer")) {
            return Err("Only assistant messages have commentary/final_answer phase".into());
        }
        normalized.native_only = true;
    }
    if rich_content(raw_content) {
        validate_native_content(raw_content, role == "assistant")?;
        normalized.native_only = true;
        if let Some(text) = raw_content
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty())
        {
            if matches!(role, "system" | "developer") {
                normalized
                    .history_instructions
                    .push(Message::text(role, text));
            } else {
                normalized.messages.push(Message::text(role, text));
            }
        }
        return Ok(());
    }
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
        Some(Value::String(detail))
            if matches!(detail.as_str(), "auto" | "low" | "high" | "original") =>
        {
            Ok(detail)
        }
        _ => Err("Image detail must be auto, low or high".into()),
    }
}

fn image_bytes(url: &str) -> Result<usize, String> {
    crate::responses_attachments::validate_data_image(url)
        .map_err(|_| "Images must use bounded PNG, JPEG, GIF or WebP base64 data URLs".into())
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
    response_value_with_include(
        request,
        id,
        created_at,
        status,
        output,
        usage,
        &request.include,
    )
}

pub fn response_value_with_include(
    request: &ResponseRequest,
    id: &str,
    created_at: u64,
    status: &str,
    output: &[Value],
    usage: Option<&Usage>,
    include: &[String],
) -> Value {
    let reported = usage.filter(|usage| usage.reported);
    let usage_value = reported.map(|usage| json!({
        "input_tokens":usage.prompt_tokens,
        "input_tokens_details":{"cached_tokens":usage.cached_tokens,"cache_write_tokens":usage.cache_write_tokens},
        "output_tokens":usage.completion_tokens,
        "output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens},
        "total_tokens":usage.total_tokens,
    }));
    let mut value = json!({
        "id":id,"object":"response","created_at":created_at,"completed_at":null,
        "status":status,"error":null,"incomplete_details":null,"output":output,
        "model":request.model,"previous_response_id":request.previous_response_id,
        "instructions":request.instructions,"tools":request.tools.iter().map(ResponseTool::public_value).collect::<Vec<_>>(),
        "tool_choice":request.tool_choice.as_ref().cloned().unwrap_or(json!("auto")),
        "parallel_tool_calls":request.parallel_tool_calls.unwrap_or(true),
        "max_output_tokens":request.max_output_tokens,"temperature":request.temperature,
        "top_p":request.top_p,"text":request.text.as_ref().cloned().unwrap_or(json!({"format":{"type":"text"}})),
        "reasoning":request.reasoning,"metadata":request.metadata,"background":request.background,
        "store":request.store,"truncation":request.truncation.as_deref().unwrap_or("disabled"),
        "conversation":request.conversation.as_ref().map(|value| if value.is_string(){json!({"id":value})}else{value.clone()}),
        "max_tool_calls":request.max_tool_calls,"top_logprobs":request.top_logprobs,
        "prompt":request.prompt,"prompt_cache_key":request.prompt_cache_key,
        "prompt_cache_options":request.prompt_cache_options,"prompt_cache_retention":request.prompt_cache_retention,
        "context_management":request.context_management,"safety_identifier":request.safety_identifier,
        "user":request.user,"moderation":request.moderation,
        "usage":null,
    });
    if let Some(items) = value["output"].as_array_mut() {
        for item in items {
            project_output_item(item, include);
        }
    }
    if let (Some(usage), Some(usage_value)) = (reported, usage_value) {
        if usage.reasoning_tokens.is_some() && usage.cache_write_tokens.is_some() {
            value["usage"] = usage_value;
        } else {
            // Current SDKs require integer reasoning and cache-write counts.
            // Unknown is not zero; preserve every known counter in the explicit
            // extension rather than creating a partially valid standard object.
            value["agents_usage"] = usage_value;
        }
    }
    value
}

/// Project only a public copy; canonical items retain their private continuation.
pub fn project_output_item(item: &mut Value, include: &[String]) {
    let included = |name: &str| include.iter().any(|value| value == name);
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") if !included("reasoning.encrypted_content") => {
            remove_field(item, "encrypted_content");
        }
        Some("file_search_call") if !included("file_search_call.results") => {
            remove_field(item, "results");
        }
        Some("code_interpreter_call") if !included("code_interpreter_call.outputs") => {
            item["outputs"] = Value::Null;
        }
        Some("web_search_call") => {
            if !included("web_search_call.results") {
                remove_field(item, "results");
            }
            if !included("web_search_call.action.sources")
                && let Some(action) = item.get_mut("action")
            {
                remove_field(action, "sources");
            }
        }
        Some("message") => {
            if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    project_content_part(part, include);
                }
            }
        }
        Some("computer_call_output") if !included("computer_call_output.output.image_url") => {
            if let Some(output) = item.get_mut("output") {
                remove_field(output, "image_url");
            }
        }
        _ => {}
    }
}

fn project_content_part(part: &mut Value, include: &[String]) {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text")
            if !include
                .iter()
                .any(|name| name == "message.output_text.logprobs") =>
        {
            if part.get("logprobs").is_some() {
                part["logprobs"] = json!([]);
            }
        }
        Some("input_image")
            if !include
                .iter()
                .any(|name| name == "message.input_image.image_url") =>
        {
            remove_field(part, "image_url");
        }
        _ => {}
    }
}

fn remove_field(value: &mut Value, field: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(field);
    }
}

/// Apply the same include boundary to canonical-ID SSE copies before fanout.
pub fn project_output_event(event: &mut Value, include: &[String]) {
    if let Some(output) = event
        .pointer_mut("/response/output")
        .and_then(Value::as_array_mut)
    {
        for item in output {
            project_output_item(item, include);
        }
    }
    if let Some(item) = event.get_mut("item") {
        project_output_item(item, include);
    }
    if let Some(part) = event.get_mut("part") {
        project_content_part(part, include);
    }
    if event.get("logprobs").is_some()
        && !include
            .iter()
            .any(|name| name == "message.output_text.logprobs")
    {
        event["logprobs"] = json!([]);
    }
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
    fn nullable_collections_and_lifecycle_flags_use_documented_defaults() {
        let request: ResponseRequest = serde_json::from_value(json!({
            "model":"agent_test","input":"hello","tools":null,"include":null,"metadata":null,
            "stream":null,"background":null,"store":null,
        }))
        .unwrap();
        request.validate().unwrap();
        assert!(request.store);
        assert!(!request.stream && !request.background);
        assert!(
            request.tools.is_empty() && request.include.is_empty() && request.metadata.is_empty()
        );
    }

    #[test]
    fn conversation_is_a_single_history_authority() {
        let mut request = request(json!("hello"));
        request.conversation = Some(json!("conv_owner"));
        assert_eq!(request.conversation_id().unwrap(), Some("conv_owner"));
        request.conversation = Some(json!({"id":"conv_owner"}));
        assert_eq!(request.conversation_id().unwrap(), Some("conv_owner"));
        request.previous_response_id = Some("resp_parent".into());
        assert!(request.validate().is_err());
        request.previous_response_id = None;
        request.conversation = Some(json!({"id":"conv_owner","owner":"forged"}));
        assert!(request.validate().is_err());
    }

    #[test]
    fn complete_controls_are_validated_without_forwarding_harness_state() {
        let request: ResponseRequest = serde_json::from_value(json!({
            "model":"agent_test","input":"hello","conversation":"conv_owner",
            "max_tool_calls":4,"top_logprobs":20,"include":["reasoning.encrypted_content"],
            "text":{"verbosity":"high"},"reasoning":{"effort":"max","summary":"detailed","context":"all_turns","mode":"pro"},
            "context_management":[{"type":"compaction","compact_threshold":12000}],
            "prompt":{"id":"pmpt_owner","version":"2","variables":{"客户":"hello"}},
            "prompt_cache_key":"stable","prompt_cache_options":{"mode":"explicit","ttl":"30m","prewarm":false},
            "prompt_cache_retention":"24h","service_tier":"priority","safety_identifier":"hashed-user",
            "user":"legacy-user","moderation":{"model":"moderation-model","policy":{"input":{"mode":"block"}}},
            "access_programs":{"cyber":"standard"},
        })).unwrap();
        request.validate().unwrap();
        let controls = request.native_controls();
        assert_eq!(controls["reasoning"]["summary"], "detailed");
        assert_eq!(controls["max_tool_calls"], 4);
        for key in [
            "model",
            "input",
            "instructions",
            "conversation",
            "previous_response_id",
            "context_management",
            "prompt",
            "store",
            "stream",
            "background",
        ] {
            assert!(controls.get(key).is_none(), "harness-owned {key}");
        }
        assert!(request.requires_native_provider());
    }

    #[test]
    fn tool_choice_resolves_against_server_registry_after_pure_validation() {
        let definitions: Vec<ToolDefinition> = serde_json::from_value(json!([
            {"type":"function","function":{"name":"time","description":"Current time","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"bash","description":"Workspace shell","parameters":{"type":"object","properties":{}}}}
        ])).unwrap();
        for choice in [
            json!("required"),
            json!({"type":"function","name":"time"}),
            json!({"type":"function","name":"bash"}),
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"time"}]}),
        ] {
            let mut request = request(json!("hello"));
            request.tool_choice = Some(choice);
            request.validate().unwrap();
            assert!(request.validate_resolved_tool_choice(&[]).is_err());
            request.validate_resolved_tool_choice(&definitions).unwrap();
        }
        let mut request = request(json!("hello"));
        request.tool_choice = Some(json!({"type":"function","name":"unregistered"}));
        request.validate().unwrap();
        assert!(request.validate_resolved_tool_choice(&definitions).is_err());
        request.tool_choice =
            Some(json!({"type":"function","name":"time","namespace":"unregistered"}));
        request.validate().unwrap();
        assert!(request.validate_resolved_tool_choice(&definitions).is_err());
        request.tool_choice = Some(json!({"type":"function","name":"__agents_code_interpreter"}));
        assert!(request.validate().is_err());
    }

    #[test]
    fn include_projection_is_explicit_idempotent_and_preserves_canonical_items() {
        let original = vec![
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"}),
            json!({"type":"compaction","id":"cmp_1","encrypted_content":"required-state"}),
            json!({"type":"file_search_call","id":"fs_1","results":[{"text":"private-evidence"}]}),
            json!({"type":"code_interpreter_call","id":"ci_1","outputs":[{"type":"logs","logs":"private-output"}]}),
            json!({"type":"web_search_call","id":"ws_1","results":[{}],"action":{"type":"search","query":"hello","sources":[{}]}}),
            json!({"type":"message","id":"msg_1","content":[{"type":"output_text","text":"answer","annotations":[],"logprobs":[{"token":"answer"}]},{"type":"input_image","image_url":"https://example.com/private.png"}]}),
            json!({"type":"computer_call_output","id":"co_1","output":{"type":"computer_screenshot","image_url":"https://example.com/screenshot.png"}}),
        ];
        let mut public = original.clone();
        for item in &mut public {
            project_output_item(item, &[]);
        }
        assert!(public[0].get("encrypted_content").is_none());
        assert_eq!(public[1], original[1]);
        assert!(public[2].get("results").is_none());
        assert!(public[3]["outputs"].is_null());
        assert!(public[4].get("results").is_none());
        assert!(public[4]["action"].get("sources").is_none());
        assert_eq!(public[5]["content"][0]["logprobs"], json!([]));
        assert!(public[5]["content"][1].get("image_url").is_none());
        assert!(public[6]["output"].get("image_url").is_none());
        let first_projection = public.clone();
        for item in &mut public {
            project_output_item(item, &[]);
        }
        assert_eq!(public, first_projection);
        assert_eq!(original[0]["encrypted_content"], "opaque");

        let includes = [
            "reasoning.encrypted_content",
            "file_search_call.results",
            "code_interpreter_call.outputs",
            "web_search_call.results",
            "web_search_call.action.sources",
            "message.output_text.logprobs",
            "message.input_image.image_url",
            "computer_call_output.output.image_url",
        ]
        .map(str::to_owned);
        let mut included = original.clone();
        for item in &mut included {
            project_output_item(item, &includes);
        }
        assert_eq!(included, original);
        let mut request = request(json!("hello"));
        let response = response_value(&request, "resp_test", 1, "completed", &original, None);
        assert_eq!(response["output"], json!(first_projection));
        request.include = includes.to_vec();
        let response = response_value(&request, "resp_test", 1, "completed", &original, None);
        assert_eq!(response["output"], json!(original));
    }

    #[test]
    fn include_projection_matches_item_part_and_delta_events() {
        let mut event = json!({"type":"response.output_item.done","sequence_number":7,
            "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]}});
        project_output_event(&mut event, &[]);
        assert!(event["item"].get("encrypted_content").is_none());
        assert_eq!(event["sequence_number"], 7);
        let mut event = json!({"type":"response.content_part.done","part":{
            "type":"output_text","text":"hello","annotations":[],"logprobs":[{}]}});
        project_output_event(&mut event, &[]);
        assert_eq!(event["part"]["logprobs"], json!([]));
        let mut event =
            json!({"type":"response.output_text.delta","delta":"hello","logprobs":[{}]});
        let original = event.clone();
        project_output_event(&mut event, &["message.output_text.logprobs".into()]);
        assert_eq!(event, original);
        project_output_event(&mut event, &[]);
        assert_eq!(event["logprobs"], json!([]));
    }

    #[test]
    fn code_network_secrets_remain_private_execution_input() {
        let secret = "domain-super-secret-value";
        let raw = json!({"type":"code_interpreter","container":{
            "type":"auto","memory_limit":"4g","network_policy":{
                "type":"allowlist","allowed_domains":["api.example.com"],
                "domain_secrets":[{"domain":"api.example.com","name":"API_TOKEN","value":secret}]
            }
        }});
        let tool: ResponseTool = serde_json::from_value(raw.clone()).unwrap();
        let mut request = request(json!("hello"));
        request.tools.push(tool.clone());
        request.validate().unwrap();
        assert_eq!(tool.to_wire_value(), raw);
        assert_eq!(serde_json::to_value(&tool).unwrap(), raw);
        let policy = tool.code_network_policy().unwrap().unwrap();
        assert!(!format!("{policy:?}").contains(secret));
        assert!(!format!("{request:?}").contains(secret));
        assert_eq!(
            serde_json::to_value(policy).unwrap(),
            raw["container"]["network_policy"]
        );
        let public = tool.public_value();
        assert!(
            public
                .pointer("/container/network_policy/domain_secrets")
                .is_none()
        );
        assert_eq!(
            public["container"]["network_policy"]["allowed_domains"],
            json!(["api.example.com"])
        );
        let response = response_value(&request, "resp_test", 1, "completed", &[], None);
        assert!(!response.to_string().contains(secret));

        let shell: ResponseTool = serde_json::from_value(json!({"type":"shell","environment":{
            "type":"container_auto","network_policy":raw["container"]["network_policy"]
        }}))
        .unwrap();
        assert!(
            shell
                .public_value()
                .pointer("/environment/network_policy/domain_secrets")
                .is_none()
        );
        assert!(!format!("{shell:?}").contains(secret));
    }

    #[test]
    fn code_network_policy_schema_errors_do_not_render_secrets() {
        let secret = "secret-invalid-input";
        for container in [
            json!({"type":"auto","network_policy":{"type":"disabled","domain_secrets":[{"domain":"api.example.com","name":"API_TOKEN","value":secret}]}}),
            json!({"type":"auto","network_policy":{"type":"allowlist","allowed_domains":["api.example.com"],"domain_secrets":[{"domain":"api.example.com","name":"API_TOKEN","value":42}]}}),
            json!({"type":"auto","network_policy":{"type":"allowlist","allowed_domains":["https://api.example.com"],"domain_secrets":[{"domain":"api.example.com","name":"API_TOKEN","value":secret}]}}),
            json!({"type":"auto","network_policy":{"type":"allowlist","allowed_domains":[] ,"domain_secrets":[{"domain":"api.example.com","name":"API_TOKEN","value":secret,"unknown":true}]}}),
            json!({"type":"auto","memory_limit":"2g"}),
            json!({"type":"unknown"}),
        ] {
            let mut request = request(json!("hello"));
            request.tools.push(
                serde_json::from_value(json!({"type":"code_interpreter","container":container}))
                    .unwrap(),
            );
            let error = request.validate().unwrap_err();
            assert!(!error.contains(secret));
        }
    }

    #[test]
    fn code_network_disabled_and_large_memory_are_valid_syntax_not_capabilities() {
        for memory in ["1g", "4g", "16g", "64g"] {
            let mut request = request(json!("hello"));
            request.tools.push(
                serde_json::from_value(json!({"type":"code_interpreter","container":{
                    "type":"auto","memory_limit":memory,"network_policy":{"type":"disabled"}
                }}))
                .unwrap(),
            );
            request.validate().unwrap();
            assert!(matches!(
                request.tools[0].code_network_policy().unwrap(),
                Some(super::CodeNetworkPolicy::Disabled {})
            ));
        }
    }

    #[test]
    fn tool_variants_round_trip_without_fake_function_fields_or_secrets() {
        for value in [
            json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g"}}),
            json!({"type":"image_generation","quality":"high","output_format":"png"}),
            json!({"type":"tool_search","execution":"server","parameters":{"type":"object"}}),
            json!({"type":"custom","name":"patch","format":{"type":"grammar","syntax":"lark","definition":"start: \"hello\""}}),
            json!({"type":"namespace","name":"project","description":"Project tools","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}),
        ] {
            let tool: ResponseTool = serde_json::from_value(value.clone()).unwrap();
            let mut request = request(json!("hello"));
            request.tools.push(tool.clone());
            request.validate().unwrap();
            assert!(request.function_definitions().unwrap().is_empty());
            assert!(request.tool_definitions().is_err());
            assert_eq!(tool.to_wire_value(), value);
            if !matches!(tool.kind(), "namespace" | "custom") {
                assert!(tool.to_wire_value().get("name").is_none());
            }
        }
        let secret = "sensitive-MCP-credential";
        let tool: ResponseTool = serde_json::from_value(json!({"type":"mcp","server_label":"owned","server_url":"https://mcp.example.com","authorization":secret,"headers":{"Authorization":secret}})).unwrap();
        assert!(!format!("{tool:?}").contains(secret));
        assert!(!tool.public_value().to_string().contains(secret));
        assert_eq!(tool.to_wire_value()["authorization"], secret);
    }

    #[test]
    fn raw_native_items_and_resolved_images_survive_structure_projection() {
        let image = json!({"type":"input_image","image_url":"data:image/png;base64,AQID","detail":"original"});
        let input = json!([
            {"type":"reasoning","id":"rs_owned","summary":[],"encrypted_content":"opaque-provider-data"},
            {"role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Planning","annotations":[]}]},
            {"role":"user","content":[image]},
            {"type":"item_reference","id":"msg_owned"},
        ]);
        let request = request(input.clone());
        let normalized = request.normalize_structure().unwrap();
        assert!(normalized.native_only);
        assert_eq!(
            request.native_input_items().unwrap(),
            input.as_array().unwrap().clone()
        );
        assert_eq!(
            normalized.native_items[2]["content"][0]["image_url"],
            image["image_url"]
        );
        assert!(normalized.input_items[3].get("status").is_none());
    }

    #[test]
    fn remote_attachment_syntax_is_public_https_and_not_a_fetch() {
        let request = request(
            json!([{"role":"user","content":[{"type":"input_file","file_url":"https://files.example.com/owned.pdf?signature=value"}]}]),
        );
        let normalized = request.normalize_structure().unwrap();
        assert!(normalized.native_only);
        assert_eq!(
            normalized.native_items[0]["content"][0]["file_url"],
            "https://files.example.com/owned.pdf?signature=value"
        );
        for url in [
            "http://files.example.com/a",
            "https://127.0.0.1/a",
            "https://localhost/a",
            "https://localhost./a",
            "https://service.internal./a",
            "https://metadata.google/a",
            "https://[::1]/a",
            "https://2130706433/a",
            "https://user:pass@files.example.com/a",
            "file:///private",
        ] {
            assert!(
                super::validate_attachment_syntax(&json!({"type":"input_file","file_url":url}))
                    .is_err()
            );
        }
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
            json!([{"type":"input_file","file_url":"https://127.0.0.1/file"}]),
            json!([{"type":"input_file","file_id":"file_one","file_data":"ignored"}]),
            json!([{"type":"input_image","file_id":"file_one","image_url":"https://127.0.0.1/a.png"}]),
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
            "https://127.0.0.1/a.png",
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
    fn inline_files_use_the_resolvers_pure_admission_contract() {
        for data in ["aGVsbG8=", "data:text/plain;base64,aGVsbG8="] {
            let request = request(
                json!([{"role":"user","content":[{"type":"input_file","filename":"notes.txt","file_data":data}]}]),
            );
            let structural = request.normalize_structure().unwrap();
            assert_eq!(
                structural.input_items[0]["content"][0]["filename"],
                "notes.txt"
            );
            assert_eq!(structural.input_items[0]["content"][0]["file_data"], data);
            assert_eq!(structural.messages[0].content, "");
            assert!(request.normalize().is_err());
        }
        for part in [
            json!({"type":"input_file","filename":"../escape","file_data":"aGVsbG8="}),
            json!({"type":"input_file","filename":"notes.txt","file_data":"invalid!"}),
            json!({"type":"input_file","file_data":"aGVsbG8="}),
        ] {
            assert!(
                request(json!([{"role":"user","content":[part]}]))
                    .normalize_structure()
                    .is_err()
            );
        }
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
    fn schema_budget_is_shared_between_all_tools_and_structured_output() {
        let mut request = request(json!("hello"));
        request.tools = (0..32).map(|index| ResponseTool {
            kind: "function".into(),
            name: format!("local_{index}"),
            description: None,
            parameters: json!({"type":"object","properties":{"name":{"type":"string","pattern":"^[a-z]+$"}}}),
            strict: None,
            options: BTreeMap::new(),
        }).collect();
        assert!(request.validate().is_ok());
        request.text = Some(
            json!({"format":{"type":"json_schema","name":"result","schema":{"type":"object","properties":{"name":{"type":"string","pattern":"^[a-z]+$"}}}}}),
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
                },
                ToolOutput {
                    call_id: "call_two".into(),
                }
            ]
        );
        let output = output_items(&normalized.messages[0], "resp_one", "completed");
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_one");
        assert_ne!(output[0]["id"], output[1]["id"]);
    }

    #[test]
    fn function_output_media_keeps_typed_history_and_native_content() {
        let output = json!([
            {"type":"input_text","text":"local chart 雪"},
            {"type":"input_image","image_url":"data:image/png;base64,AQID"}
        ]);
        let normalized = request(json!([{
            "type":"function_call_output","call_id":"call_chart","output":output
        }]))
        .normalize()
        .unwrap();
        assert_eq!(normalized.messages[0].role, "tool");
        assert_eq!(
            normalized.messages[0].tool_call_id.as_deref(),
            Some("call_chart")
        );
        assert_eq!(
            normalized.messages[0].content[0],
            json!({"type":"text","text":"local chart 雪"})
        );
        assert_eq!(
            normalized.messages[0].content[1]["image_url"]["detail"],
            "auto"
        );
        assert_eq!(normalized.input_items[0]["output"][0], output[0]);
        assert_eq!(
            normalized.input_items[0]["output"][1]["type"],
            "input_image"
        );
        assert_eq!(normalized.input_items[0]["output"][1]["detail"], "auto");
    }

    #[test]
    fn function_output_files_use_the_same_preflight_and_aggregate_media_limits() {
        let valid = request(json!([{
            "type":"function_call_output","call_id":"call_chart","output":[
                {"type":"input_file","file_id":"file_owned"},
                {"type":"input_image","file_id":"file_chart","detail":"low"}
            ]
        }]));
        let normalized = valid.normalize_structure().unwrap();
        assert_eq!(
            normalized.input_items[0]["output"][0]["file_id"],
            "file_owned"
        );
        assert_eq!(
            normalized.input_items[0]["output"][1]["file_id"],
            "file_chart"
        );
        assert!(
            valid.normalize().is_err(),
            "File references must be resolved before inference"
        );
        for part in [
            json!({"type":"input_image","image_url":"https://127.0.0.1/a.png"}),
            json!({"type":"input_file","file_id":"../private"}),
            json!({"type":"input_image","file_id":"file_chart","detail":"invalid"}),
        ] {
            assert!(
                request(
                    json!([{"type":"function_call_output","call_id":"call_chart","output":[part]}])
                )
                .normalize_structure()
                .is_err()
            );
        }
        let images = vec![json!({"type":"input_image","file_id":"file_chart"}); MAX_IMAGES];
        assert!(
            request(json!([
                {"role":"user","content":[{"type":"input_image","file_id":"file_chart"}]},
                {"type":"function_call_output","call_id":"call_chart","output":images}
            ]))
            .normalize_structure()
            .is_err()
        );
    }

    #[test]
    fn duplicate_outputs_and_unsupported_input_fail_before_execution() {
        for input in [
            json!([{"type":"function_call_output","call_id":"call_one","output":"a"},{"type":"function_call_output","call_id":"call_one","output":"b"}]),
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://127.0.0.1/a.png"}]}]),
            json!([{"type":"made_up_item","id":"item_old"}]),
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
            json!({"include":["made_up_projection"]}),
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
    fn usage_requires_authoritative_reasoning_and_cache_write_for_the_standard_shape() {
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
        assert!(value["usage"].is_null());
        assert_eq!(
            value["agents_usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
        assert!(value["agents_usage"]["input_tokens_details"]["cache_write_tokens"].is_null());
        usage.cache_write_tokens = Some(3);
        usage.reasoning_tokens = None;
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert!(value["usage"].is_null());
        assert_eq!(
            value["agents_usage"]["input_tokens_details"]["cache_write_tokens"],
            3
        );
        usage.reasoning_tokens = Some(2);
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
        assert!(value.get("agents_usage").is_none());
        usage.reasoning_tokens = Some(0);
        usage.cache_write_tokens = Some(0);
        let value = response_value(&request, "resp_one", 1, "completed", &[], Some(&usage));
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
        assert_eq!(
            value["usage"]["input_tokens_details"]["cache_write_tokens"],
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
