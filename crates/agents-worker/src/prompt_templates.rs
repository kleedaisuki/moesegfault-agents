//! Owner-scoped, code-managed prompt compatibility. Registry data is private.
use std::collections::{BTreeMap, BTreeSet};

use minijinja::{Environment, UndefinedBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::Env;

use crate::responses_wire::ResponseRequest;

const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const MAX_RENDERED_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_VARIABLES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromptError {
    pub status: u16,
    pub code: &'static str,
}

fn invalid() -> PromptError {
    PromptError {
        status: 400,
        code: "invalid_prompt_variables",
    }
}

fn registry_error() -> PromptError {
    PromptError {
        status: 503,
        code: "prompt_registry_invalid",
    }
}

fn absent() -> PromptError {
    PromptError {
        status: 404,
        code: "prompt_not_found",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    format: u8,
    prompts: Vec<Prompt>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    owner: String,
    id: String,
    current: String,
    versions: BTreeMap<String, Template>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Template {
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    input: Vec<TemplateMessage>,
    #[serde(default)]
    variables: BTreeMap<String, VariableKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateMessage {
    role: String,
    content: Vec<TemplatePart>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TemplatePart {
    InputText { text: String },
    Variable { name: String },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VariableKind {
    Text,
    Image,
    File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    variables: Option<BTreeMap<String, Value>>,
}

/// An accepted response owns the rendered version; later registry deployments
/// cannot change retries, continuation history or compaction of that response.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ResolvedPrompt {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub instructions: Option<String>,
    pub input: Vec<Value>,
}

impl std::fmt::Debug for ResolvedPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedPrompt")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl ResolvedPrompt {
    pub(crate) fn for_successor(&self) -> Self {
        // An automatic successor inherits settings, not already accepted source
        // input. The digest still identifies the original resolved template.
        Self {
            input: Vec::new(),
            ..self.clone()
        }
    }

    pub(crate) fn apply(&self, raw: &ResponseRequest) -> Result<ResponseRequest, PromptError> {
        let mut effective = raw.clone();
        effective.prompt = None;
        if effective.instructions.is_none() {
            effective.instructions = self.instructions.clone();
        }
        let mut input = self.input.clone();
        match &raw.input {
            Value::Null => (),
            Value::String(text) => input.push(json!({"role":"user","content":text})),
            Value::Array(items) => input.extend(items.iter().cloned()),
            _ => return Err(invalid()),
        }
        effective.input = Value::Array(input);
        effective.validate().map_err(|_| invalid())?;
        Ok(effective)
    }

    pub(crate) fn public_reference(&self, raw: &Value) -> Value {
        let mut reference = raw.clone();
        reference["version"] = json!(self.version);
        reference
    }
}

pub(crate) fn resolve(
    env: &Env,
    owner: &str,
    request: &ResponseRequest,
) -> Result<Option<ResolvedPrompt>, PromptError> {
    let Some(reference) = &request.prompt else {
        return Ok(None);
    };
    let configured = env
        .secret("PROMPT_REGISTRY_JSON")
        .map(|value| value.to_string())
        .or_else(|_| {
            env.var("PROMPT_REGISTRY_JSON")
                .map(|value| value.to_string())
        })
        .map_err(|_| PromptError {
            status: 503,
            code: "prompt_registry_unconfigured",
        })?;
    from_registry(&configured, owner, reference).map(Some)
}

fn from_registry(
    registry: &str,
    owner: &str,
    reference: &Value,
) -> Result<ResolvedPrompt, PromptError> {
    if registry.len() > MAX_REGISTRY_BYTES {
        return Err(registry_error());
    }
    let reference: Reference = serde_json::from_value(reference.clone()).map_err(|_| invalid())?;
    let registry: Registry = serde_json::from_str(registry).map_err(|_| registry_error())?;
    if registry.format != 1 || registry.prompts.len() > 256 {
        return Err(registry_error());
    }
    let mut identities = BTreeSet::new();
    for prompt in &registry.prompts {
        if !identities.insert((&prompt.owner, &prompt.id))
            || !name(&prompt.id)
            || prompt.owner.is_empty()
            || prompt.versions.len() > 64
            || !prompt.versions.contains_key(&prompt.current)
            || prompt.versions.keys().any(|version| !name(version))
        {
            return Err(registry_error());
        }
    }
    let prompt = registry
        .prompts
        .iter()
        .find(|prompt| prompt.owner == owner && prompt.id == reference.id)
        .ok_or_else(absent)?;
    let version = reference.version.as_deref().unwrap_or(&prompt.current);
    let template = prompt.versions.get(version).ok_or_else(absent)?;
    let values = reference.variables.unwrap_or_default();
    validate_variables(template, &values)?;
    let text_values = text_variables(&values);
    let mut engine = Environment::new();
    engine.set_undefined_behavior(UndefinedBehavior::Strict);
    engine.set_fuel(Some(100_000));
    let instructions = template
        .instructions
        .as_ref()
        .map(|text| render(&engine, text, &text_values))
        .transpose()?;
    if template.input.len() > 128 {
        return Err(registry_error());
    }
    let mut input = Vec::with_capacity(template.input.len());
    for message in &template.input {
        input.push(render_message(&engine, message, &values, &text_values)?);
    }
    let bytes = serde_json::to_vec(&json!({"instructions":instructions,"input":input}))
        .map_err(|_| registry_error())?;
    if bytes.len() > MAX_RENDERED_BYTES {
        return Err(invalid());
    }
    Ok(ResolvedPrompt {
        id: prompt.id.clone(),
        version: version.into(),
        digest: format!("{:x}", Sha256::digest(bytes)),
        instructions,
        input,
    })
}

fn name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn validate_variables(
    template: &Template,
    values: &BTreeMap<String, Value>,
) -> Result<(), PromptError> {
    if template.variables.len() > MAX_VARIABLES
        || template.variables.keys().any(|name| !self::name(name))
        || template.variables.len() != values.len()
        || values
            .keys()
            .any(|name| !template.variables.contains_key(name))
    {
        return Err(invalid());
    }
    for (name, kind) in &template.variables {
        let value = values.get(name).ok_or_else(invalid)?;
        let expected = match kind {
            VariableKind::Text if value.is_string() => {
                if value.as_str().unwrap().len() > MAX_TEXT_BYTES {
                    return Err(invalid());
                }
                continue;
            }
            VariableKind::Text => "input_text",
            VariableKind::Image => "input_image",
            VariableKind::File => "input_file",
        };
        if value["type"] != expected {
            return Err(invalid());
        }
        validate_part(value)?;
    }
    Ok(())
}

fn validate_part(part: &Value) -> Result<(), PromptError> {
    let request: ResponseRequest = serde_json::from_value(json!({
        "model":"prompt_validation", "input":[{"role":"user","content":[part]}]
    }))
    .map_err(|_| invalid())?;
    request.validate().map_err(|_| invalid())
}

fn text_variables(values: &BTreeMap<String, Value>) -> BTreeMap<String, String> {
    values
        .iter()
        .filter_map(|(name, value)| {
            value
                .as_str()
                .or_else(|| {
                    (value["type"] == "input_text"
                        && value.get("prompt_cache_breakpoint").is_none())
                    .then(|| value["text"].as_str())
                    .flatten()
                })
                .map(|text| (name.clone(), text.into()))
        })
        .collect()
}

fn render(
    engine: &Environment<'_>,
    template: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, PromptError> {
    if template.len() > MAX_TEXT_BYTES {
        return Err(registry_error());
    }
    let template = engine
        .template_from_str(template)
        .map_err(|_| registry_error())?;
    let mut output = BoundedOutput(Vec::new());
    template
        .render_captured_to(minijinja::value::Serde(values), &mut output)
        .map_err(|_| invalid())?;
    String::from_utf8(output.0).map_err(|_| invalid())
}

struct BoundedOutput(Vec<u8>);

impl std::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_TEXT_BYTES {
            return Err(std::io::Error::other("prompt output exceeds limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn render_message(
    engine: &Environment<'_>,
    message: &TemplateMessage,
    values: &BTreeMap<String, Value>,
    text_values: &BTreeMap<String, String>,
) -> Result<Value, PromptError> {
    if !["user", "assistant", "system", "developer"].contains(&message.role.as_str())
        || message.content.len() > 128
    {
        return Err(registry_error());
    }
    let mut parts = Vec::with_capacity(message.content.len());
    for part in &message.content {
        let value = match part {
            TemplatePart::InputText { text } => {
                json!({"type":"input_text","text":render(engine, text, text_values)?})
            }
            TemplatePart::Variable { name } => {
                let value = values.get(name).ok_or_else(invalid)?;
                match value.as_str() {
                    Some(text) => json!({"type":"input_text","text":text}),
                    None => value.clone(),
                }
            }
        };
        validate_part(&value)?;
        parts.push(value);
    }
    Ok(json!({"role":message.role,"content":parts}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Value {
        json!({"format":1,"prompts":[{"owner":"owner","id":"pmpt_support","current":"2","versions":{
            "1":{"instructions":"Old {{customer}}","variables":{"customer":"text"},"input":[]},
            "2":{"instructions":"Help {{customer}}","variables":{"customer":"text","photo":"image"},"input":[
                {"role":"user","content":[{"type":"input_text","text":"Customer: {{customer}}"},{"type":"variable","name":"photo"}]}
            ]}
        }}]})
    }

    fn reference() -> Value {
        json!({"id":"pmpt_support","variables":{"customer":"Klee <3 {{ignored}}","photo":{"type":"input_image","image_url":"data:image/png;base64,AA=="}}})
    }

    #[test]
    fn current_version_is_rendered_once_with_typed_parts_and_no_recursive_evaluation() {
        let resolved = from_registry(&registry().to_string(), "owner", &reference()).unwrap();
        assert_eq!(resolved.version, "2");
        assert_eq!(
            resolved.instructions.as_deref(),
            Some("Help Klee <3 {{ignored}}")
        );
        assert_eq!(
            resolved.input[0]["content"][1],
            reference()["variables"]["photo"]
        );
        assert_eq!(
            resolved.input[0]["content"][0]["text"],
            "Customer: Klee <3 {{ignored}}"
        );
        assert_eq!(resolved.public_reference(&reference())["version"], "2");
    }

    #[test]
    fn owner_and_missing_version_are_indistinguishable() {
        let registry = registry().to_string();
        assert_eq!(
            from_registry(&registry, "other", &reference()).err(),
            Some(absent())
        );
        let mut reference = reference();
        reference["version"] = json!("missing");
        assert_eq!(
            from_registry(&registry, "owner", &reference).err(),
            Some(absent())
        );
    }

    #[test]
    fn explicit_version_and_durable_replay_ignore_a_later_current_version() {
        let reference = json!({"id":"pmpt_support","version":"1","variables":{"customer":"Klee"}});
        let saved = from_registry(&registry().to_string(), "owner", &reference).unwrap();
        let bytes = serde_json::to_vec(&saved).unwrap();
        let mut changed = registry();
        changed["prompts"][0]["current"] = json!("1");
        changed["prompts"][0]["versions"]["1"]["instructions"] = json!("Changed {{customer}}");
        let replay: ResolvedPrompt = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(replay.instructions.as_deref(), Some("Old Klee"));
        assert_eq!(replay.digest, saved.digest);
        assert_ne!(
            from_registry(&changed.to_string(), "owner", &reference)
                .unwrap()
                .digest,
            replay.digest
        );
    }

    #[test]
    fn execution_request_does_not_mutate_raw_identity_or_inherit_template_instructions() {
        let resolved = from_registry(&registry().to_string(), "owner", &reference()).unwrap();
        let raw: ResponseRequest = serde_json::from_value(json!({"model":"agent","input":"New input","instructions":"Explicit","prompt":reference()})).unwrap();
        let identity = serde_json::to_vec(&raw).unwrap();
        let effective = resolved.apply(&raw).unwrap();
        assert!(effective.prompt.is_none());
        assert_eq!(effective.instructions.as_deref(), Some("Explicit"));
        assert_eq!(effective.input.as_array().unwrap().len(), 2);
        assert_eq!(effective.input[1]["content"], "New input");
        assert_eq!(identity, serde_json::to_vec(&raw).unwrap());
    }

    #[test]
    fn missing_extra_and_wrong_typed_variables_are_rejected() {
        for mutation in 0..3 {
            let mut reference = reference();
            match mutation {
                0 => {
                    reference["variables"]
                        .as_object_mut()
                        .unwrap()
                        .remove("photo");
                }
                1 => {
                    reference["variables"]["extra"] = json!("x");
                }
                _ => {
                    reference["variables"]["photo"] =
                        json!({"type":"input_file","file_id":"file_x"});
                }
            }
            assert_eq!(
                from_registry(&registry().to_string(), "owner", &reference).err(),
                Some(invalid())
            );
        }
    }

    #[test]
    fn duplicate_registry_identity_and_unknown_schema_are_rejected() {
        let mut data = registry();
        let duplicate = data["prompts"][0].clone();
        data["prompts"].as_array_mut().unwrap().push(duplicate);
        assert_eq!(
            from_registry(&data.to_string(), "owner", &reference()).err(),
            Some(registry_error())
        );
        let mut data = registry();
        data["prompts"][0]["unsafe"] = json!(true);
        assert_eq!(
            from_registry(&data.to_string(), "owner", &reference()).err(),
            Some(registry_error())
        );
    }

    #[test]
    fn rendered_output_is_bounded_before_allocating_an_amplified_result() {
        let mut data = registry();
        data["prompts"][0]["versions"]["1"]["instructions"] = json!("{{customer}}{{customer}}");
        let reference = json!({"id":"pmpt_support","version":"1", "variables":{"customer":"x".repeat(MAX_TEXT_BYTES)}});
        assert_eq!(
            from_registry(&data.to_string(), "owner", &reference).err(),
            Some(invalid())
        );
    }

    #[test]
    fn automatic_successor_preserves_resolved_settings_without_reapplying_source() {
        let resolved = from_registry(&registry().to_string(), "owner", &reference()).unwrap();
        let inherited = resolved.for_successor();
        assert!(inherited.input.is_empty());
        assert_eq!(inherited.digest, resolved.digest);
        assert_eq!(inherited.version, "2");
        let raw: ResponseRequest = serde_json::from_value(json!({
            "model":"agent", "input":"Accepted steering", "prompt":reference()
        }))
        .unwrap();
        let effective = inherited.apply(&raw).unwrap();
        assert_eq!(effective.input.as_array().unwrap().len(), 1);
        assert_eq!(effective.input[0]["content"], "Accepted steering");
        assert_eq!(effective.instructions, resolved.instructions);
        assert_eq!(inherited.for_successor().digest, resolved.digest);
    }

    proptest::proptest! {
        #[test]
        fn variable_content_is_literal_not_a_second_template(value in ".{0,64}") {
            let reference=json!({"id":"pmpt_support","version":"1","variables":{"customer":value}});
            let result=from_registry(&registry().to_string(),"owner",&reference).unwrap();
            let expected = format!("Old {}",reference["variables"]["customer"].as_str().unwrap());
            proptest::prop_assert_eq!(result.instructions.as_deref(),Some(expected.as_str()));
        }
    }
}
