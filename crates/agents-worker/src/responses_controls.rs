//! Syntax of public controls, separate from model/provider capability checks.

use super::{ResponseRequest, ResponseTool, bounded_text, identifier, object_keys};
use serde_json::Value;

const INCLUDES: &[&str] = &[
    "file_search_call.results",
    "web_search_call.results",
    "web_search_call.action.sources",
    "message.input_image.image_url",
    "computer_call_output.output.image_url",
    "code_interpreter_call.outputs",
    "reasoning.encrypted_content",
    "message.output_text.logprobs",
];

pub(super) fn validate(request: &ResponseRequest) -> Result<(), String> {
    conversation(request)?;
    if request.top_logprobs.is_some_and(|value| value > 20) {
        return Err("top_logprobs must be between zero and twenty".into());
    }
    if request
        .include
        .iter()
        .any(|value| !INCLUDES.contains(&value.as_str()))
    {
        return Err("Unknown Responses include projection".into());
    }
    string_choice(
        request.service_tier.as_deref(),
        &[
            "auto",
            "default",
            "flex",
            "scale",
            "priority",
            "fast",
            "ultrafast",
        ],
        "service_tier",
    )?;
    string_choice(
        request.prompt_cache_retention.as_deref(),
        &["in_memory", "24h"],
        "prompt_cache_retention",
    )?;
    for (value, label, limit) in [
        (&request.safety_identifier, "safety_identifier", 128),
        (&request.prompt_cache_key, "prompt_cache_key", 512),
        (&request.user, "user", 512),
    ] {
        if let Some(value) = value {
            bounded_text(value, limit, label)?;
        }
    }
    if let Some(value) = &request.reasoning {
        reasoning(value)?;
    }
    if let Some(value) = &request.context_management {
        context(value)?;
    }
    if let Some(value) = &request.prompt_cache_options {
        cache(value)?;
    }
    if let Some(value) = &request.prompt {
        prompt(value)?;
    }
    if let Some(value) = &request.moderation {
        moderation(value)?;
    }
    if let Some(value) = &request.access_programs {
        object_keys(value, &["cyber"], "access_programs")?;
        choice(
            value,
            "cyber",
            &["standard", "daybreak_blue", "daybreak_red"],
        )?;
    }
    if let Some(value) = &request.tool_choice {
        tool_choice(value, &request.tools)?;
    }
    Ok(())
}

fn conversation(request: &ResponseRequest) -> Result<(), String> {
    let Some(value) = &request.conversation else {
        return Ok(());
    };
    if request.previous_response_id.is_some() {
        return Err("conversation and previous_response_id cannot be combined".into());
    }
    let id = match value {
        Value::String(value) => value.as_str(),
        Value::Object(_) => {
            object_keys(value, &["id"], "conversation")?;
            value
                .get("id")
                .and_then(Value::as_str)
                .ok_or("conversation requires an ID")?
        }
        _ => return Err("conversation must be an ID or an object containing its ID".into()),
    };
    identifier(id, "conversation ID")
}

fn string_choice(value: Option<&str>, allowed: &[&str], label: &str) -> Result<(), String> {
    if value.is_some_and(|value| !allowed.contains(&value)) {
        return Err(format!("Invalid {label}"));
    }
    Ok(())
}

fn choice(value: &Value, key: &str, allowed: &[&str]) -> Result<(), String> {
    match value.get(key).filter(|value| !value.is_null()) {
        None => Ok(()),
        Some(Value::String(value)) if allowed.contains(&value.as_str()) => Ok(()),
        _ => Err(format!("Invalid {key}")),
    }
}

fn boolean(value: &Value, key: &str) -> Result<(), String> {
    if value
        .get(key)
        .is_some_and(|value| !value.is_boolean() && !value.is_null())
    {
        return Err(format!("{key} must be boolean"));
    }
    Ok(())
}

fn reasoning(value: &Value) -> Result<(), String> {
    object_keys(
        value,
        &["context", "effort", "generate_summary", "mode", "summary"],
        "reasoning",
    )?;
    choice(value, "context", &["auto", "current_turn", "all_turns"])?;
    choice(
        value,
        "effort",
        &["none", "minimal", "low", "medium", "high", "xhigh", "max"],
    )?;
    choice(value, "summary", &["auto", "concise", "detailed"])?;
    choice(value, "generate_summary", &["auto", "concise", "detailed"])?;
    if let Some(mode) = value.get("mode").filter(|value| !value.is_null()) {
        bounded_text(
            mode.as_str().ok_or("reasoning.mode must be text")?,
            128,
            "reasoning.mode",
        )?;
    }
    if value.get("summary").is_some_and(|value| !value.is_null())
        && value
            .get("generate_summary")
            .is_some_and(|value| !value.is_null())
    {
        return Err("Use reasoning.summary or generate_summary, not both".into());
    }
    Ok(())
}

fn context(value: &Value) -> Result<(), String> {
    let entries = value
        .as_array()
        .filter(|entries| entries.len() <= 16)
        .ok_or("context_management must be a bounded array")?;
    for entry in entries {
        object_keys(
            entry,
            &["type", "compact_threshold"],
            "context_management entry",
        )?;
        if entry["type"] != "compaction" {
            return Err("Unknown context-management operation".into());
        }
        if entry
            .get("compact_threshold")
            .is_some_and(|value| !value.is_null() && value.as_u64().is_none_or(|value| value == 0))
        {
            return Err("compact_threshold must be a positive integer".into());
        }
    }
    Ok(())
}

fn cache(value: &Value) -> Result<(), String> {
    object_keys(
        value,
        &["comparison_response_id", "mode", "prewarm", "ttl"],
        "prompt_cache_options",
    )?;
    choice(value, "mode", &["implicit", "explicit"])?;
    choice(value, "ttl", &["30m"])?;
    boolean(value, "prewarm")?;
    if let Some(id) = value
        .get("comparison_response_id")
        .filter(|value| !value.is_null())
    {
        identifier(
            id.as_str().ok_or("comparison_response_id must be text")?,
            "comparison_response_id",
        )?;
    }
    Ok(())
}

fn prompt(value: &Value) -> Result<(), String> {
    object_keys(value, &["id", "version", "variables"], "prompt")?;
    identifier(
        value["id"].as_str().ok_or("prompt requires an ID")?,
        "prompt ID",
    )?;
    if let Some(version) = value.get("version").filter(|value| !value.is_null()) {
        bounded_text(
            version.as_str().ok_or("prompt version must be text")?,
            128,
            "prompt version",
        )?;
    }
    if let Some(variables) = value.get("variables").filter(|value| !value.is_null()) {
        let variables = variables
            .as_object()
            .filter(|variables| variables.len() <= 128)
            .ok_or("prompt variables must be a bounded map")?;
        for (name, value) in variables {
            if name.is_empty() {
                return Err("Prompt variable names must not be empty".into());
            }
            bounded_text(name, 128, "prompt variable name")?;
            if value.is_string() {
                bounded_text(value.as_str().unwrap(), 256 * 1024, "prompt variable")?;
            } else if !matches!(
                value.get("type").and_then(Value::as_str),
                Some("input_text" | "input_image" | "input_file")
            ) {
                return Err("Invalid prompt variable content".into());
            }
        }
    }
    Ok(())
}

fn moderation(value: &Value) -> Result<(), String> {
    object_keys(value, &["model", "policy"], "moderation")?;
    bounded_text(
        value["model"]
            .as_str()
            .ok_or("moderation model is required")?,
        128,
        "moderation model",
    )?;
    if let Some(policy) = value.get("policy").filter(|value| !value.is_null()) {
        object_keys(policy, &["input", "output"], "moderation policy")?;
        for name in ["input", "output"] {
            if let Some(entry) = policy.get(name).filter(|value| !value.is_null()) {
                object_keys(entry, &["mode"], "moderation policy entry")?;
                if entry.get("mode").is_none() {
                    return Err("moderation policy mode is required".into());
                }
                choice(entry, "mode", &["score", "block"])?;
            }
        }
    }
    Ok(())
}

pub(super) fn tool_choice(value: &Value, tools: &[ResponseTool]) -> Result<(), String> {
    if let Some(choice) = value.as_str() {
        if choice == "required" && tools.is_empty() {
            return Err("Required tool_choice needs an available tool".into());
        }
        return string_choice(Some(choice), &["none", "auto", "required"], "tool_choice");
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or("tool_choice requires a type")?;
    if kind == "allowed_tools" {
        object_keys(value, &["type", "mode", "tools"], "allowed_tools")?;
        choice(value, "mode", &["auto", "required"])?;
        let subset = value["tools"]
            .as_array()
            .filter(|items| !items.is_empty() && items.len() <= 128)
            .ok_or("allowed_tools requires a nonempty tool subset")?;
        for item in subset {
            if item["type"] == "allowed_tools" {
                return Err("Allowed tool subsets cannot contain another subset".into());
            }
            tool_choice(item, tools)?;
        }
        return Ok(());
    }
    let name = value.get("name").and_then(Value::as_str);
    if matches!(kind, "function" | "custom") {
        object_keys(value, &["type", "name", "namespace"], "named tool_choice")?;
        let name = name.ok_or("Named tool_choice requires a name")?;
        let available = if let Some(namespace) = value.get("namespace").and_then(Value::as_str) {
            tools
                .iter()
                .filter(|tool| tool.kind == "namespace" && tool.name == namespace)
                .any(|tool| {
                    tool.options
                        .get("tools")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| item["type"] == kind && item["name"] == name)
                        })
                })
        } else {
            tools
                .iter()
                .any(|tool| tool.kind == kind && tool.name == name)
        };
        return if available {
            Ok(())
        } else {
            Err("tool_choice names an unavailable tool".into())
        };
    }
    if !tools.iter().any(|tool| tool.kind == kind) {
        return Err("tool_choice selects an unavailable tool kind".into());
    }
    Ok(())
}

pub(super) fn builtin_tool(
    tool: &ResponseTool,
    schemas: &mut crate::responses_schema::SchemaBudget,
) -> Result<(), String> {
    let raw = tool.to_wire_value();
    let allowed: &[&str] = match tool.kind.as_str() {
        "custom" => &[
            "type",
            "name",
            "description",
            "allowed_callers",
            "async",
            "defer_loading",
            "format",
        ],
        "namespace" => &["type", "name", "description", "tools"],
        "file_search" => &[
            "type",
            "vector_store_ids",
            "filters",
            "max_num_results",
            "ranking_options",
        ],
        "computer" | "local_shell" | "programmatic_tool_calling" => &["type"],
        "computer_use_preview" => &["type", "display_height", "display_width", "environment"],
        "web_search" | "web_search_2025_08_26" => &[
            "type",
            "external_web_access",
            "filters",
            "search_context_size",
            "user_location",
        ],
        "web_search_preview" | "web_search_preview_2025_03_11" => &[
            "type",
            "search_content_types",
            "search_context_size",
            "user_location",
        ],
        "mcp" => &[
            "type",
            "server_label",
            "allowed_callers",
            "allowed_tools",
            "authorization",
            "connector_id",
            "defer_loading",
            "headers",
            "require_approval",
            "server_description",
            "server_url",
            "tunnel_id",
        ],
        "code_interpreter" => &["type", "container", "allowed_callers"],
        "image_generation" => &[
            "type",
            "action",
            "background",
            "input_fidelity",
            "input_image_mask",
            "model",
            "moderation",
            "output_compression",
            "output_format",
            "partial_images",
            "quality",
            "size",
        ],
        "shell" => &["type", "allowed_callers", "environment"],
        "tool_search" => &["type", "description", "execution", "parameters"],
        "apply_patch" => &["type", "allowed_callers"],
        _ => return Err("Unknown Responses tool type".into()),
    };
    object_keys(&raw, allowed, "tool")?;
    if matches!(tool.kind.as_str(), "custom" | "namespace") {
        identifier(&tool.name, "tool name")?;
    }
    callers(&raw)?;
    for name in ["async", "defer_loading", "external_web_access"] {
        boolean(&raw, name)?;
    }
    if tool.kind == "namespace" {
        namespace(tool, schemas)?;
    }
    if tool.kind == "custom" {
        custom(&raw)?;
    }
    if tool.kind == "file_search" {
        string_array(&raw["vector_store_ids"], 128, "vector_store_ids")?;
    }
    if tool.kind == "code_interpreter"
        && !raw["container"].is_string()
        && !raw["container"].is_object()
    {
        return Err("code_interpreter requires a container".into());
    }
    if tool.kind == "mcp"
        && (!raw["server_label"].is_string()
            || (raw.get("server_url").is_none() && raw.get("connector_id").is_none()))
    {
        return Err("MCP requires a server label and URL or connector".into());
    }
    if tool.kind == "computer_use_preview"
        && (!raw["display_width"].as_u64().is_some_and(|value| value > 0)
            || !raw["display_height"]
                .as_u64()
                .is_some_and(|value| value > 0))
    {
        return Err("Computer display dimensions must be positive integers".into());
    }
    Ok(())
}

fn string_array(value: &Value, maximum: usize, label: &str) -> Result<(), String> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= maximum)
        .ok_or_else(|| format!("{label} must be a bounded array"))?;
    if values.iter().any(|value| !value.is_string()) {
        return Err(format!("{label} entries must be strings"));
    }
    Ok(())
}

pub(super) fn callers(value: &Value) -> Result<(), String> {
    if let Some(callers) = value
        .get("allowed_callers")
        .filter(|value| !value.is_null())
    {
        string_array(callers, 2, "allowed_callers")?;
        if callers
            .as_array()
            .unwrap()
            .iter()
            .any(|value| !matches!(value.as_str(), Some("direct" | "programmatic")))
        {
            return Err("Unknown tool caller context".into());
        }
    }
    Ok(())
}

fn namespace(
    tool: &ResponseTool,
    schemas: &mut crate::responses_schema::SchemaBudget,
) -> Result<(), String> {
    if tool.description.is_none() {
        return Err("Namespace description is required".into());
    }
    let items = tool
        .options
        .get("tools")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= 128)
        .ok_or("Namespace tools must be a bounded array")?;
    let mut names = std::collections::BTreeSet::new();
    for item in items {
        if !matches!(item["type"].as_str(), Some("function" | "custom")) {
            return Err("A namespace contains only function/custom tools".into());
        }
        let nested: ResponseTool =
            serde_json::from_value(item.clone()).map_err(|_| "Invalid namespace tool")?;
        identifier(&nested.name, "namespace tool name")?;
        if !names.insert(nested.name.clone()) {
            return Err("Namespace tool names must be unique".into());
        }
        if nested.kind == "function" {
            super::validate_function(&nested, schemas, 128)?;
        } else {
            builtin_tool(&nested, schemas)?;
        }
    }
    Ok(())
}

fn custom(value: &Value) -> Result<(), String> {
    let Some(format) = value.get("format") else {
        return Ok(());
    };
    match format["type"].as_str() {
        Some("text") => object_keys(format, &["type"], "custom format"),
        Some("grammar") => {
            object_keys(format, &["type", "syntax", "definition"], "custom grammar")?;
            choice(format, "syntax", &["lark", "regex"])?;
            if format.get("syntax").is_none() {
                return Err("Custom grammar syntax is required".into());
            }
            bounded_text(
                format["definition"]
                    .as_str()
                    .ok_or("Custom grammar definition is required")?,
                64 * 1024,
                "grammar",
            )
        }
        _ => Err("Unknown custom tool format".into()),
    }
}
